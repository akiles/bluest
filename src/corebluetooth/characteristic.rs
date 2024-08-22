use futures_core::Stream;
use futures_lite::StreamExt;
use objc_foundation::{INSData, INSFastEnumeration};
use objc_id::ShareId;

use super::delegates::{PeripheralDelegate, PeripheralEvent};
use super::types::{CBCharacteristic, CBCharacteristicWriteType, CBPeripheralState};
use crate::error::ErrorKind;
use crate::util::defer;
use crate::{Characteristic, CharacteristicProperties, Descriptor, Error, Result, Uuid};

/// A Bluetooth GATT characteristic
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CharacteristicImpl {
    inner: ShareId<CBCharacteristic>,
    delegate: ShareId<PeripheralDelegate>,
}

impl Characteristic {
    pub(super) fn new(characteristic: &CBCharacteristic, delegate: ShareId<PeripheralDelegate>) -> Self {
        Characteristic(CharacteristicImpl {
            inner: unsafe { ShareId::from_ptr(characteristic as *const _ as *mut _) },
            delegate,
        })
    }
}

impl CharacteristicImpl {
    /// The [`Uuid`] identifying the type of this GATT characteristic
    pub fn uuid(&self) -> Uuid {
        self.inner.uuid().to_uuid()
    }

    /// The [`Uuid`] identifying the type of this GATT characteristic
    pub async fn uuid_async(&self) -> Result<Uuid> {
        Ok(self.uuid())
    }

    /// The properties of this this GATT characteristic.
    ///
    /// Characteristic properties indicate which operations (e.g. read, write, notify, etc) may be performed on this
    /// characteristic.
    pub async fn properties(&self) -> Result<CharacteristicProperties> {
        Ok(self.inner.properties().into())
    }

    /// The cached value of this characteristic
    ///
    /// If the value has not yet been read, this method may either return an error or perform a read of the value.
    pub async fn value(&self) -> Result<Vec<u8>> {
        self.inner.value().map(|val| val.bytes().to_vec()).ok_or_else(|| {
            Error::new(
                ErrorKind::NotReady,
                None,
                "the characteristic value has not been read".to_string(),
            )
        })
    }

    /// Read the value of this characteristic from the device
    pub async fn read(&self) -> Result<Vec<u8>> {
        let service = self.inner.service();
        let peripheral = service.peripheral();
        let mut receiver = self.delegate.sender().new_receiver();

        if peripheral.state() != CBPeripheralState::CONNECTED {
            return Err(ErrorKind::NotConnected.into());
        }

        peripheral.read_characteristic_value(&self.inner);

        loop {
            match receiver.recv().await.map_err(Error::from_recv_error)? {
                PeripheralEvent::CharacteristicValueUpdate { characteristic, error }
                    if characteristic == self.inner =>
                {
                    match error {
                        Some(err) => return Err(Error::from_nserror(err)),
                        None => return self.value().await,
                    }
                }
                PeripheralEvent::Disconnected { error } => {
                    return Err(Error::from_kind_and_nserror(ErrorKind::NotConnected, error));
                }
                PeripheralEvent::ServicesChanged { invalidated_services }
                    if invalidated_services.contains(&service) =>
                {
                    return Err(ErrorKind::ServiceChanged.into());
                }
                _ => (),
            }
        }
    }

    /// Write the value of this descriptor on the device to `value` and request the device return a response indicating
    /// a successful write.
    pub async fn write(&self, value: &[u8]) -> Result<()> {
        let service = self.inner.service();
        let peripheral = service.peripheral();
        let mut receiver = self.delegate.sender().new_receiver();

        if peripheral.state() != CBPeripheralState::CONNECTED {
            return Err(ErrorKind::NotConnected.into());
        }

        let data = INSData::from_vec(value.to_vec());
        peripheral.write_characteristic_value(&self.inner, &data, CBCharacteristicWriteType::WithResponse);

        loop {
            match receiver.recv().await.map_err(Error::from_recv_error)? {
                PeripheralEvent::CharacteristicValueWrite { characteristic, error } if characteristic == self.inner => {
                    match error {
                        Some(err) => return Err(Error::from_nserror(err)),
                        None => return Ok(()),
                    }
                }
                PeripheralEvent::Disconnected { error } => {
                    return Err(Error::from_kind_and_nserror(ErrorKind::NotConnected, error));
                }
                PeripheralEvent::ServicesChanged { invalidated_services }
                    if invalidated_services.contains(&service) =>
                {
                    return Err(ErrorKind::ServiceChanged.into());
                }
                _ => (),
            }
        }
    }

    /// Write the value of this descriptor on the device to `value` without requesting a response.
    pub async fn write_without_response(&self, value: &[u8]) -> Result<()> {
        let service = self.inner.service();
        let peripheral = service.peripheral();
        let mut receiver = self.delegate.sender().new_receiver();

        if peripheral.state() != CBPeripheralState::CONNECTED {
            return Err(ErrorKind::NotConnected.into());
        } else if !peripheral.can_send_write_without_response() {
            while let Ok(evt) = receiver.recv().await {
                match evt {
                    PeripheralEvent::ReadyToWrite => break,
                    PeripheralEvent::Disconnected { error } => {
                        return Err(Error::from_kind_and_nserror(ErrorKind::NotConnected, error));
                    }
                    PeripheralEvent::ServicesChanged { invalidated_services }
                        if invalidated_services.contains(&service) =>
                    {
                        return Err(ErrorKind::ServiceChanged.into());
                    }
                    _ => (),
                }
            }
        }

        let data = INSData::from_vec(value.to_vec());
        peripheral.write_characteristic_value(&self.inner, &data, CBCharacteristicWriteType::WithoutResponse);
        Ok(())
    }

    /// Get the maximum amount of data that can be written in a single packet for this characteristic.
    pub fn max_write_len(&self) -> Result<usize> {
        let peripheral = self.inner.service().peripheral();
        Ok(peripheral.maximum_write_value_length_for_type(CBCharacteristicWriteType::WithoutResponse))
    }

    /// Get the maximum amount of data that can be written in a single packet for this characteristic.
    pub async fn max_write_len_async(&self) -> Result<usize> {
        self.max_write_len()
    }

    /// Enables notification of value changes for this GATT characteristic.
    ///
    /// Returns a stream of values for the characteristic sent from the device.
    pub async fn notify(&self) -> Result<impl Stream<Item = Result<Vec<u8>>> + Send + Unpin + '_> {
        let properties = self.properties().await?;
        if !(properties.notify || properties.indicate) {
            return Err(Error::new(
                ErrorKind::NotSupported,
                None,
                "characteristic does not support indications or notifications".to_string(),
            ));
        };

        let service = self.inner.service();
        let peripheral = service.peripheral();
        let mut receiver = self.delegate.sender().new_receiver();

        if peripheral.state() != CBPeripheralState::CONNECTED {
            return Err(ErrorKind::NotConnected.into());
        }

        peripheral.set_notify(&self.inner, true);
        let guard = defer(move || {
            let peripheral = self.inner.service().peripheral();
            peripheral.set_notify(&self.inner, false);
        });

        loop {
            match receiver.recv().await.map_err(Error::from_recv_error)? {
                PeripheralEvent::NotificationStateUpdate { characteristic, error } if characteristic == self.inner => {
                    match error {
                        Some(err) => return Err(Error::from_nserror(err)),
                        None => break,
                    }
                }
                PeripheralEvent::Disconnected { error } => {
                    return Err(Error::from_kind_and_nserror(ErrorKind::NotConnected, error));
                }
                PeripheralEvent::ServicesChanged { invalidated_services }
                    if invalidated_services.contains(&service) =>
                {
                    return Err(ErrorKind::ServiceChanged.into());
                }
                _ => (),
            }
        }

        let updates = receiver
            .filter_map(move |x| {
                let _guard = &guard;
                match x {
                    PeripheralEvent::CharacteristicValueUpdate { characteristic, error }
                        if characteristic == self.inner =>
                    {
                        match error {
                            Some(err) => Some(Err(Error::from_nserror(err))),
                            None => Some(Ok(())),
                        }
                    }
                    PeripheralEvent::Disconnected { error } => {
                        Some(Err(Error::from_kind_and_nserror(ErrorKind::NotConnected, error)))
                    }
                    PeripheralEvent::ServicesChanged { invalidated_services }
                        if invalidated_services.contains(&service) =>
                    {
                        Some(Err(ErrorKind::ServiceChanged.into()))
                    }
                    _ => None,
                }
            })
            .then(move |x| {
                Box::pin(async move {
                    match x {
                        Ok(_) => self.value().await,
                        Err(err) => Err(err),
                    }
                })
            });

        Ok(updates)
    }

    /// Is the device currently sending notifications for this characteristic?
    pub async fn is_notifying(&self) -> Result<bool> {
        Ok(self.inner.is_notifying())
    }

    /// Discover the descriptors associated with this characteristic.
    pub async fn discover_descriptors(&self) -> Result<Vec<Descriptor>> {
        let service = self.inner.service();
        let peripheral = service.peripheral();
        let mut receiver = self.delegate.sender().new_receiver();

        if peripheral.state() != CBPeripheralState::CONNECTED {
            return Err(ErrorKind::NotConnected.into());
        }

        peripheral.discover_descriptors(&self.inner);

        loop {
            match receiver.recv().await.map_err(Error::from_recv_error)? {
                PeripheralEvent::DiscoveredDescriptors { characteristic, error } if characteristic == self.inner => {
                    match error {
                        Some(err) => return Err(Error::from_nserror(err)),
                        None => break,
                    }
                }
                PeripheralEvent::Disconnected { error } => {
                    return Err(Error::from_kind_and_nserror(ErrorKind::NotConnected, error));
                }
                PeripheralEvent::ServicesChanged { invalidated_services }
                    if invalidated_services.contains(&service) =>
                {
                    return Err(ErrorKind::ServiceChanged.into());
                }
                _ => (),
            }
        }

        self.descriptors_inner()
    }

    /// Get previously discovered descriptors.
    ///
    /// If no descriptors have been discovered yet, this method will perform descriptor discovery.
    pub async fn descriptors(&self) -> Result<Vec<Descriptor>> {
        match self.descriptors_inner() {
            Ok(descriptors) => Ok(descriptors),
            Err(_) => self.discover_descriptors().await,
        }
    }

    fn descriptors_inner(&self) -> Result<Vec<Descriptor>> {
        self.inner
            .descriptors()
            .map(|s| {
                s.enumerator()
                    .map(|x| Descriptor::new(x, self.delegate.clone()))
                    .collect()
            })
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotReady,
                    None,
                    "no descriptors have been discovered".to_string(),
                )
            })
    }
}
