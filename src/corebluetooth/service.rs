use objc_foundation::{INSArray, INSFastEnumeration, NSArray};
use objc_id::{Id, ShareId};

use super::characteristic::Characteristic;
use super::delegates::PeripheralEvent;
use super::types::{CBService, CBUUID};
use crate::error::ErrorKind;
use crate::{Error, Result, Uuid};

/// A Bluetooth GATT service
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Service {
    inner: ShareId<CBService>,
}

impl Service {
    pub(super) fn new(service: &CBService) -> Self {
        Service {
            inner: unsafe { ShareId::from_ptr(service as *const _ as *mut _) },
        }
    }

    /// The [Uuid] identifying the type of this GATT service
    pub fn uuid(&self) -> Uuid {
        self.inner.uuid().to_uuid()
    }

    /// Whether this is a primary service of the device.
    ///
    /// # Platform specific
    ///
    /// This method is available on MacOS/iOS only.
    pub fn is_primary(&self) -> bool {
        self.inner.is_primary()
    }

    /// Discover all characteristics associated with this service.
    pub async fn discover_characteristics(&self) -> Result<Vec<Characteristic>> {
        self.discover_characteristics_inner(None).await
    }

    /// Discover the characteristic(s) with the given [Uuid].
    pub async fn discover_characteristics_with_uuid(&self, uuid: Uuid) -> Result<Vec<Characteristic>> {
        let uuids = {
            let vec = vec![CBUUID::from_uuid(uuid)];
            NSArray::from_vec(vec)
        };

        self.discover_characteristics_inner(Some(uuids)).await
    }

    async fn discover_characteristics_inner(&self, uuids: Option<Id<NSArray<CBUUID>>>) -> Result<Vec<Characteristic>> {
        let peripheral = self.inner.peripheral();
        let mut receiver = peripheral.subscribe()?;
        peripheral.discover_characteristics(&self.inner, uuids);

        loop {
            match receiver.recv().await.map_err(Error::from_recv_error)? {
                PeripheralEvent::DiscoveredCharacteristics { service, error } if service == self.inner => match error {
                    Some(err) => return Err(Error::from_nserror(err)),
                    None => break,
                },
                PeripheralEvent::ServicesChanged { invalidated_services }
                    if invalidated_services.contains(&self.inner) =>
                {
                    return Err(ErrorKind::ServiceChanged.into());
                }
                _ => (),
            }
        }

        self.characteristics().await
    }

    /// Get previously discovered characteristics.
    ///
    /// If no characteristics have been discovered yet, this method may either perform characteristic discovery or
    /// return an error.
    pub async fn characteristics(&self) -> Result<Vec<Characteristic>> {
        self.inner
            .characteristics()
            .map(|s| s.enumerator().map(Characteristic::new).collect())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotReady,
                    None,
                    "no characteristics have been discovered".to_string(),
                )
            })
    }

    /// Discover the included services of this service.
    pub async fn discover_included_services(&self) -> Result<Vec<Service>> {
        self.discover_included_services_inner(None).await
    }

    /// Discover the included service(s) with the given [Uuid].
    pub async fn discover_included_services_with_uuid(&self, uuid: Uuid) -> Result<Vec<Service>> {
        let uuids = {
            let vec = vec![CBUUID::from_uuid(uuid)];
            NSArray::from_vec(vec)
        };

        self.discover_included_services_inner(Some(uuids)).await
    }

    async fn discover_included_services_inner(&self, uuids: Option<Id<NSArray<CBUUID>>>) -> Result<Vec<Service>> {
        let peripheral = self.inner.peripheral();
        let mut receiver = peripheral.subscribe()?;
        peripheral.discover_included_services(&self.inner, uuids);

        loop {
            match receiver.recv().await.map_err(Error::from_recv_error)? {
                PeripheralEvent::DiscoveredIncludedServices { service, error } if service == self.inner => {
                    match error {
                        Some(err) => return Err(Error::from_nserror(err)),
                        None => break,
                    }
                }
                PeripheralEvent::ServicesChanged { invalidated_services }
                    if invalidated_services.contains(&self.inner) =>
                {
                    return Err(ErrorKind::ServiceChanged.into());
                }
                _ => (),
            }
        }

        self.included_services().await
    }

    /// Get previously discovered included services.
    ///
    /// If no included services have been discovered yet, this method may either perform included service discovery
    /// or return an error.
    pub async fn included_services(&self) -> Result<Vec<Service>> {
        self.inner
            .included_services()
            .map(|s| s.enumerator().map(Service::new).collect())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotReady,
                    None,
                    "no included services have been discovered".to_string(),
                )
            })
    }
}
