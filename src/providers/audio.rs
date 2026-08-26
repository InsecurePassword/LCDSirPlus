//! Windows default audio endpoint state via Core Audio COM:
//! output volume/mute and default microphone mute.

#![cfg(windows)]

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{EDataFlow, ERole, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct AudioState {
    pub volume_percent: f64,
    pub mic_known: bool,
    pub mic_muted: bool,
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
        // Already-initialized COM on this thread is fine; a different model
        // (RPC_E_CHANGED_MODE) also works for our purposes.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
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
