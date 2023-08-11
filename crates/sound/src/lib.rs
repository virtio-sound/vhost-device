// SPDX-License-Identifier: Apache-2.0 or BSD-3-Clause

mod audio_backends;
mod vhu_sound;
mod virtio_sound;

use std::io::{Error as IoError, ErrorKind};
use std::sync::Arc;

use log::{info, warn};
use thiserror::Error as ThisError;
use vhost::{vhost_user, vhost_user::Listener};
use vhost_user_backend::VhostUserDaemon;
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap, Le32, VolatileSlice};

use crate::vhu_sound::VhostUserSoundBackend;

pub type Result<T> = std::result::Result<T, Error>;

/// Custom error types
#[derive(Debug, ThisError)]
pub enum Error {
    #[error("Failed to handle event other than EPOLLIN event")]
    HandleEventNotEpollIn,
    #[error("Failed to handle unknown event")]
    HandleUnknownEvent,
    #[error("Failed to create a new EventFd")]
    EventFdCreate(IoError),
    #[error("Request missing data buffer")]
    SoundReqMissingData,
    #[error("Audio backend not supported")]
    AudioBackendNotSupported,
    #[error("Descriptor not found")]
    DescriptorNotFound,
    #[error("Descriptor read failed")]
    DescriptorReadFailed,
    #[error("Descriptor write failed")]
    DescriptorWriteFailed,
    #[error("Isufficient descriptor size, required: {0}, found: {1}")]
    InsufficientDescriptorSize(usize, usize),
    #[error("Failed to send notification")]
    SendNotificationFailed,
    #[error("Stream with id {0} not found")]
    StreamWithIdNotFound(u32),
    #[error("Stream Listener with id {0} not found")]
    StreamListenerWithIdNotFound(u32),
    #[error("Invalid descriptor count {0}")]
    UnexpectedDescriptorCount(usize),
    #[error("Invalid descriptor size, expected: {0}, found: {1}")]
    UnexpectedDescriptorSize(usize, usize),
    #[error("Invalid descriptor size, expected at least: {0}, found: {1}")]
    UnexpectedMinimumDescriptorSize(usize, usize),
    #[error("Received unexpected readable descriptor at index {0}")]
    UnexpectedReadableDescriptor(usize),
    #[error("Received unexpected write only descriptor at index {0}")]
    UnexpectedWriteOnlyDescriptor(usize),
}

impl std::convert::From<Error> for IoError {
    fn from(e: Error) -> Self {
        IoError::new(ErrorKind::Other, e)
    }
}

#[derive(Debug, Default, Clone)]
pub struct PCMParams {
    pub features: Le32,
    /// size of hardware buffer in bytes
    pub buffer_bytes: Le32,
    /// size of hardware period in bytes
    pub period_bytes: Le32,
    pub channels: u8,
    pub format: u8,
    pub rate: u8,
    pub direction: u8,
}

#[derive(Debug, Clone)]
/// This structure is the public API through which an external program
/// is allowed to configure the backend.
pub struct SoundConfig {
    /// vhost-user Unix domain socket
    socket: String,
    /// use multiple threads to hanlde the virtqueues
    multi_thread: bool,
    /// audio backend name
    audio_backend_name: String,
}

impl SoundConfig {
    /// Create a new instance of the SoundConfig struct, containing the
    /// parameters to be fed into the sound-backend server.
    pub fn new(socket: String, multi_thread: bool, audio_backend_name: String) -> Self {
        Self {
            socket,
            multi_thread,
            audio_backend_name,
        }
    }

    /// Return the path of the unix domain socket which is listening to
    /// requests from the guest.
    pub fn get_socket_path(&self) -> String {
        String::from(&self.socket)
    }
}

pub type SoundBitmap = ();

#[derive(Debug)]
pub struct SoundRequest<'a> {
    data_slice: Option<VolatileSlice<'a, SoundBitmap>>,
}

impl<'a> SoundRequest<'a> {
    pub fn data_slice(&self) -> Option<&VolatileSlice<'a, SoundBitmap>> {
        self.data_slice.as_ref()
    }
}

/// This is the public API through which an external program starts the
/// vhost-user-sound backend server.
pub fn start_backend_server(config: SoundConfig) {
    let listener = Listener::new(config.get_socket_path(), true).unwrap();
    let backend = Arc::new(VhostUserSoundBackend::new(config).unwrap());

    let mut daemon = VhostUserDaemon::new(
        String::from("vhost-user-sound"),
        backend.clone(),
        GuestMemoryAtomic::new(GuestMemoryMmap::<SoundBitmap>::new()),
    )
    .unwrap();

    daemon.start(listener).unwrap();

    match daemon.wait() {
        Ok(()) => {
            info!("Stopping cleanly");
        }
        Err(vhost_user_backend::Error::HandleRequest(vhost_user::Error::PartialMessage)) => {
            info!("vhost-user connection closed with partial message. If the VM is shutting down, this is expected behavior; otherwise, it might be a bug.");
        }
        Err(e) => {
            warn!("Error running daemon: {:?}", e);
        }
    }

    // No matter the result, we need to shut down the worker thread.
    backend.send_exit_event();
}
