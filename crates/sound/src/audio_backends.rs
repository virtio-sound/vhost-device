// SPDX-License-Identifier: Apache-2.0 or BSD-3-Clause

#[cfg(feature = "null-backend")]
mod null;

#[cfg(feature = "pw-backend")]
mod pw_backend;

#[cfg(feature = "null-backend")]
use self::null::NullBackend;
use self::pw_backend::PwBackend;
#[cfg(feature = "pw-backend")]
use crate::PCMParams;
use crate::{Error, Result};

pub trait AudioBackend {
    fn get_stream_state(&self, _stream_id: u32) -> Result<u32>;

    // TODO: to remove mut
    fn write(&self, stream_id: u32, req: &mut [u8]) -> Result<u32>;

    fn read(&self, stream_id: u32) -> Result<()>;

    fn set_param(&self, _stream_id: u32, _params: PCMParams) -> Result<()> {
        Ok(())
    }

    fn prepare(&self, _stream_id: u32) -> Result<()> {
        Ok(())
    }

    fn release(&self, _stream_id: u32) -> Result<()> {
        Ok(())
    }

    fn start(&self, _stream_id: u32) -> Result<()> {
        Ok(())
    }

    fn stop(&self, _stream_id: u32) -> Result<()> {
        Ok(())
    }
}

pub fn alloc_audio_backend(name: String) -> Result<Box<dyn AudioBackend + Send + Sync>> {
    match name.as_str() {
        #[cfg(feature = "null-backend")]
        "null" => Ok(Box::new(NullBackend::new())),
        #[cfg(feature = "pw-backend")]
        "pipewire" => Ok(Box::new(PwBackend::new())),
        _ => Err(Error::AudioBackendNotSupported),
    }
}
