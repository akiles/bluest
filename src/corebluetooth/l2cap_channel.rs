use std::fmt;
use std::sync::Arc;
use async_channel::{Receiver, Sender, TryRecvError, TrySendError};
use std::thread;
use std::time::Duration;

use crate::Result;
use super::types::{CBL2CAPChannel, NSStreamStatus}; // Adjust the import based on your project structure
use crate::error::{Error, ErrorKind};
use objc_id::{Id, Shared};
use tracing::debug;

/// Utility struct to close the channel on drop.
pub(super) struct L2capCloser {
    channel: Id<CBL2CAPChannel, Shared>,
}

impl L2capCloser {
    fn close(&self) {
        self.channel.input_stream().close();
        self.channel.output_stream().close();
    }
}

impl Drop for L2capCloser {
    fn drop(&mut self) {
        self.close()
    }
}

/// The reader side of an L2CAP channel.
pub struct L2capChannelReader {
    stream: Receiver<Vec<u8>>,
    closer: Arc<L2capCloser>,
}

impl L2capChannelReader {
    /// Creates a new L2capChannelReader.
    pub fn new(channel: Id<CBL2CAPChannel, Shared>) -> Self {
        let (sender, receiver) = async_channel::bounded(16);
        let closer = Arc::new(L2capCloser { channel: channel.clone() });
        
        channel.input_stream().open();

        thread::spawn(move || read_loop(channel, sender));

        Self {
            stream: receiver,
            closer,
        }
    }

    /// Reads data from the L2CAP channel into the provided buffer.
    #[inline]
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let packet = self.stream
            .recv()
            .await
            .map_err(|_| Error::new(ErrorKind::ConnectionFailed, None, "channel is closed".to_string()))?;

        if packet.len() > buf.len() {
            return Err(Error::new(
                ErrorKind::InvalidParameter,
                None,
                "Buffer is too small".to_string(),
            ));
        }

        buf[..packet.len()].copy_from_slice(&packet);
        Ok(packet.len())
    }

    /// Attempts to read data from the L2CAP channel into the provided buffer without blocking.
    #[inline]
    pub fn try_read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let packet = self.stream.try_recv().map_err(|e| match e {
            TryRecvError::Empty => Error::new(ErrorKind::NotReady, None, "no received packet in queue".to_string()),
            TryRecvError::Closed => Error::new(ErrorKind::ConnectionFailed, None, "channel is closed".to_string()),
        })?;

        if packet.len() > buf.len() {
            return Err(Error::new(
                ErrorKind::InvalidParameter,
                None,
                "Buffer is too small".to_string(),
            ));
        }

        buf[..packet.len()].copy_from_slice(&packet);
        Ok(packet.len())
    }

    /// Closes the L2CAP channel reader.
    pub async fn close(&mut self) -> Result<()> {
        self.closer.close();
        Ok(())
    }
}

impl fmt::Debug for L2capChannelReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("L2capChannelReader")
    }
}

/// The writer side of an L2CAP channel.
pub struct L2capChannelWriter {
    stream: Sender<Vec<u8>>,
    closer: Arc<L2capCloser>,
}

impl L2capChannelWriter {
    /// Creates a new L2capChannelWriter.
    pub fn new(channel: Id<CBL2CAPChannel, Shared>) -> Self {
        let (sender, receiver) = async_channel::bounded(16);
        let closer = Arc::new(L2capCloser { channel: channel.clone() });
        
        channel.output_stream().open();

        thread::spawn(move || write_loop(channel, receiver));

        Self {
            stream: sender,
            closer,
        }

    }

    /// Writes data to the L2CAP channel.
    pub async fn write(&mut self, packet: &[u8]) -> Result<()> {
        self.stream
            .send(packet.to_vec())
            .await
            .map_err(|_| Error::new(ErrorKind::ConnectionFailed, None, "channel is closed".to_string()))
    }

    /// Attempts to write data to the L2CAP channel without blocking.
    pub fn try_write(&mut self, packet: &[u8]) -> Result<()> {
        self.stream.try_send(packet.to_vec()).map_err(|e| match e {
            TrySendError::Closed(_) => Error::new(ErrorKind::ConnectionFailed, None, "channel is closed".to_string()),
            TrySendError::Full(_) => Error::new(ErrorKind::NotReady, None, "No buffer space for write".to_string()),
        })
    }

    /// Closes the L2CAP channel writer.
    pub async fn close(&mut self) -> Result<()> {
        self.closer.close();
        Ok(())
    }
}

impl fmt::Debug for L2capChannelWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("L2capChannelWriter")
    }
}

/// Continuously reads from the L2CAP channel and sends the data through the provided sender.
///
/// This function is used to handle the asynchronous nature of L2CAP communication.
/// It runs in a separate thread to continuously poll the input stream for new data,
/// allowing the main thread to perform other tasks without blocking on I/O operations.
/// This approach improves responsiveness and efficiency in handling incoming data.
fn read_loop(channel: Id<CBL2CAPChannel, Shared>, sender: Sender<Vec<u8>>) {
    #[allow(unused_mut)] 
    let mut buf = vec![0u8; 1024];
    loop {
        let stream_status = channel.input_stream().stream_status();
        debug!("Read Loop Stream Status: {:?}", stream_status);
        
        if stream_status == NSStreamStatus(0) || stream_status == NSStreamStatus(6) {
            break;
        }
        if !channel.input_stream().has_bytes_available() || stream_status != NSStreamStatus(2) {
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        let res = channel.input_stream().read(buf.as_ptr(), buf.len());
        if res < 0 {
            debug!("Read Loop Error: Stream read failed");
            break;
        }
        let size = res.try_into().unwrap();
        let packet = unsafe { Vec::<u8>::from_raw_parts(buf.as_ptr() as *mut u8, size, size) };
        if sender.send_blocking(packet.to_vec()).is_err() {
            debug!("Read Loop Error: Sender is closed");
            break;
        }
        core::mem::forget(packet);
    }
    debug!("rx_thread_end");
}

/// Continuously receives data from the provided receiver and writes it to the L2CAP channel.
///
/// This function is used for managing outgoing data in a non-blocking manner.
/// By running in a separate thread, it allows the main application to queue up data
/// for sending without waiting for each write operation to complete. This improves
/// overall performance and responsiveness, especially when dealing with potentially
/// slow or unreliable Bluetooth connections.
fn write_loop(channel: Id<CBL2CAPChannel, Shared>, receiver: Receiver<Vec<u8>>) {
    while let Ok(packet) = receiver.recv_blocking() {
        let objc_packet = unsafe { core::slice::from_raw_parts(packet.as_ptr() as *const u8, packet.len()) };
        let res = channel.output_stream().write(objc_packet, packet.len());
        if res < 0 {
            debug!("Write Loop Error: Stream write failed");
            break;
        }
    }
    debug!("tx_thread_end");
}
