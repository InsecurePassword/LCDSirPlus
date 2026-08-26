//! Windows default audio endpoint state via Core Audio COM:
//! output volume/mute and default microphone mute.

#![cfg(windows)]

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{EDataFlow, ERole, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct AudioState {
    pub volume_percent: f64,
    pub mic_known: bool,
    pub mic_muted: bool,
}

pub struct ComThread {
    uninitialize: bool,
}

impl Drop for ComThread {
    fn drop(&mut self) {
        if self.uninitialize {
            unsafe { CoUninitialize() };
        }
    }
}

/// Initialize COM once for the provider thread. S_OK and S_FALSE both require
/// a matching CoUninitialize; RPC_E_CHANGED_MODE means COM is already usable.
pub fn initialize_thread() -> Result<ComThread, String> {
    let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if result.is_ok() {
        Ok(ComThread { uninitialize: true })
    } else if result == RPC_E_CHANGED_MODE {
        Ok(ComThread {
            uninitialize: false,
        })
    } else {
        Err(format!(
            "initialize COM: {}",
            windows::core::Error::from(result)
        ))
    }
}

fn endpoint_volume(dataflow: EDataFlow) -> windows::core::Result<IAudioEndpointVolume> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(dataflow, ERole(1))?; // eMultimedia
        device.Activate(CLSCTX_ALL, None)
    }
}

pub fn read() -> Result<AudioState, String> {
    unsafe {
        let output =
            endpoint_volume(EDataFlow(0)).map_err(|e| format!("output endpoint: {}", e))?;
        let volume = output
            .GetMasterVolumeLevelScalar()
            .map_err(|e| format!("volume: {}", e))?;
        let _muted = output.GetMute().map_err(|e| format!("mute: {}", e))?;
        let (mic_known, mic_muted) = match endpoint_volume(EDataFlow(1)) {
            Ok(mic) => match mic.GetMute() {
                Ok(m) => (true, m.as_bool()),
                Err(_) => (false, false),
            },
            Err(_) => (false, false),
        };
        Ok(AudioState {
            volume_percent: (f64::from(volume) * 100.0).clamp(0.0, 100.0),
            mic_known,
            mic_muted,
        })
    }
}
