//! Read-only native GPU telemetry through the vendor-installed NVAPI/ADLX DLLs.

#![cfg(windows)]
#![allow(dead_code)]

use std::ffi::{c_char, c_void};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null_mut, NonNull};
use std::time::SystemTime;

use windows::core::{s, w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

use crate::model::{Metric, MetricKey, Reading};

const NVAPI_OK: i32 = 0;
const NVAPI_MAX_PHYSICAL_GPUS: usize = 64;

#[derive(Clone, Debug)]
pub struct Sample {
    pub backend: String,
    pub readings: Vec<Reading>,
    pub temperature: Metric,
    /// Total board power only. Chip-only power is never published as board power.
    pub power_w: Option<f64>,
    pub sampled_at: SystemTime,
}

pub fn candidates(selector: &str) -> &'static [&'static str] {
    match selector {
        "auto" => &["nvapi", "adlx"],
        "nvapi" => &["nvapi"],
        "adlx" => &["adlx"],
        _ => &[],
    }
}

pub struct Provider {
    selector: String,
    backend: Option<Backend>,
}

impl Provider {
    pub fn new(selector: &str) -> Self {
        Self {
            selector: selector.to_string(),
            backend: None,
        }
    }

    pub fn sample(&mut self, selector: &str) -> Result<Sample, String> {
        if selector != self.selector {
            self.selector = selector.to_string();
            self.backend = None;
        }
        if candidates(selector).is_empty() {
            return Err("native GPU telemetry disabled".into());
        }
        if let Some(backend) = &mut self.backend {
            if let Ok(sample) = backend.sample() {
                return Ok(sample);
            }
            self.backend = None;
        }

        let mut failures = Vec::new();
        for name in candidates(selector) {
            let opened = match *name {
                "nvapi" => Nvapi::open().map(Backend::Nvapi),
                "adlx" => Adlx::open().map(Backend::Adlx),
                _ => unreachable!(),
            };
            match opened {
                Ok(mut backend) => match backend.sample() {
                    Ok(sample) => {
                        self.backend = Some(backend);
                        return Ok(sample);
                    }
                    Err(e) => failures.push(format!("{name}: {e}")),
                },
                Err(e) => failures.push(format!("{name}: {e}")),
            }
        }
        Err(failures.join("; "))
    }
}

enum Backend {
    Nvapi(Nvapi),
    Adlx(Adlx),
}

impl Backend {
    fn sample(&mut self) -> Result<Sample, String> {
        match self {
            Backend::Nvapi(v) => v.sample(),
            Backend::Adlx(v) => v.sample(),
        }
    }
}

type NvStatus = i32;
type NvHandle = *mut c_void;
type NvQuery = unsafe extern "C" fn(u32) -> *mut c_void;
type NvInitialize = unsafe extern "C" fn() -> NvStatus;
type NvUnload = unsafe extern "C" fn() -> NvStatus;
type NvEnumerate = unsafe extern "C" fn(*mut NvHandle, *mut u32) -> NvStatus;
type NvDynamic = unsafe extern "C" fn(NvHandle, *mut NvDynamicPstates) -> NvStatus;
type NvThermal = unsafe extern "C" fn(NvHandle, u32, *mut NvThermalSettings) -> NvStatus;
type NvMemory = unsafe extern "C" fn(NvHandle, *mut NvMemoryInfo) -> NvStatus;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvUtilization {
    present: u32,
    percentage: u32,
}

#[repr(C)]
#[derive(Default)]
struct NvDynamicPstates {
    version: u32,
    flags: u32,
    utilization: [NvUtilization; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvThermalSensor {
    controller: i32,
    default_min: i32,
    default_max: i32,
    current: i32,
    target: i32,
}

#[repr(C)]
#[derive(Default)]
struct NvThermalSettings {
    version: u32,
    count: u32,
    sensor: [NvThermalSensor; 3],
}

#[repr(C)]
#[derive(Default)]
struct NvMemoryInfo {
    version: u32,
    dedicated: u64,
    available: u64,
    system: u64,
    shared: u64,
    current_available: u64,
    eviction_size: u64,
    eviction_count: u64,
    promotion_size: u64,
    promotion_count: u64,
}

fn nv_version<T>(version: u32) -> u32 {
    size_of::<T>() as u32 | (version << 16)
}

fn valid_percent(value: f64) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}

fn valid_temperature(value: f64) -> bool {
    value.is_finite() && (-50.0..=200.0).contains(&value)
}

fn adlx_board_power(status: AdlxResult, value: f64) -> Option<f64> {
    (status == 0 && value.is_finite() && value >= 0.0).then_some(value)
}

fn project(
    backend: &str,
    utilization: Option<f64>,
    temperature: Option<f64>,
    vram_used: Option<u64>,
    vram_total: Option<u64>,
    power_w: Option<f64>,
    at: SystemTime,
) -> Sample {
    let hardware = format!("{backend}:gpu0");
    let mut readings = Vec::new();
    if let Some(value) = utilization.filter(|v| valid_percent(*v)) {
        readings.push(Reading::current_percent(
            MetricKey::GPUUtilization,
            value,
            &hardware,
            at,
        ));
    }
    if let Some(total) = vram_total.filter(|v| *v > 0) {
        readings.push(Reading::current_bytes(
            MetricKey::VRAMTotal,
            total,
            &hardware,
            at,
        ));
        if let Some(used) = vram_used.filter(|v| *v <= total) {
            readings.push(Reading::current_bytes(
                MetricKey::VRAMUsed,
                used,
                &hardware,
                at,
            ));
            readings.push(Reading::current_percent(
                MetricKey::VRAMUtilization,
                used as f64 * 100.0 / total as f64,
                &hardware,
                at,
            ));
        }
    }
    Sample {
        backend: backend.into(),
        readings,
        temperature: temperature
            .filter(|v| valid_temperature(*v))
            .map(|v| Metric::valid(v, at))
            .unwrap_or_default(),
        power_w: power_w.filter(|value| value.is_finite() && *value >= 0.0),
        sampled_at: at,
    }
}

struct Nvapi {
    module: HMODULE,
    unload: NvUnload,
    handles: Vec<NvHandle>,
    dynamic: NvDynamic,
    thermal: NvThermal,
    memory: NvMemory,
    nvml: Option<Nvml>,
}

impl Nvapi {
    fn open() -> Result<Self, String> {
        unsafe {
            let module = LoadLibraryExW(w!("nvapi64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
                .map_err(|e| format!("load trusted nvapi64.dll: {e}"))?;
            let mut cleanup: Option<NvUnload> = None;
            let result = (|| {
                let query: NvQuery = proc(module, s!("nvapi_QueryInterface"))?;
                let initialize: NvInitialize = nv_proc(query, 0x0150_e828, "Initialize")?;
                let unload: NvUnload = nv_proc(query, 0xd22b_dd7e, "Unload")?;
                status(initialize(), "NvAPI_Initialize")?;
                cleanup = Some(unload);
                let enumerate: NvEnumerate = nv_proc(query, 0xe5ac_921f, "EnumPhysicalGPUs")?;
                let mut handles = [null_mut(); NVAPI_MAX_PHYSICAL_GPUS];
                let mut count = 0u32;
                status(
                    enumerate(handles.as_mut_ptr(), &mut count),
                    "NvAPI_EnumPhysicalGPUs",
                )?;
                if count == 0 || count as usize > handles.len() {
                    return Err(format!("NVAPI returned invalid GPU count {count}"));
                }
                Ok(Self {
                    module,
                    unload,
                    handles: handles[..count as usize].to_vec(),
                    dynamic: nv_proc(query, 0x60de_d2ed, "GPU_GetDynamicPstatesInfoEx")?,
                    thermal: nv_proc(query, 0xe364_0a56, "GPU_GetThermalSettings")?,
                    memory: nv_proc(query, 0xc059_9498, "GPU_GetMemoryInfoEx")?,
                    nvml: Nvml::open().ok(),
                })
            })();
            if result.is_err() {
                if let Some(unload) = cleanup {
                    let _ = unload();
                }
                let _ = FreeLibrary(module);
            }
            result
        }
    }

    fn sample(&mut self) -> Result<Sample, String> {
        let handle = self.handles[0];
        let at = SystemTime::now();
        unsafe {
            let mut dynamic = NvDynamicPstates {
                version: nv_version::<NvDynamicPstates>(1),
                ..Default::default()
            };
            let utilization = if (self.dynamic)(handle, &mut dynamic) == NVAPI_OK
                && dynamic.utilization[0].present & 1 != 0
            {
                Some(dynamic.utilization[0].percentage as f64)
            } else {
                None
            };

            let mut thermal = NvThermalSettings {
                version: nv_version::<NvThermalSettings>(2),
                ..Default::default()
            };
            let temperature = if (self.thermal)(handle, 15, &mut thermal) == NVAPI_OK {
                thermal.sensor[..(thermal.count as usize).min(thermal.sensor.len())]
                    .iter()
                    .find(|s| s.target == 1)
                    .map(|s| s.current as f64)
            } else {
                None
            };

            let mut memory = NvMemoryInfo {
                version: nv_version::<NvMemoryInfo>(1),
                ..Default::default()
            };
            let (used, total) = if (self.memory)(handle, &mut memory) == NVAPI_OK
                && memory.dedicated > 0
                && memory.current_available <= memory.dedicated
            {
                (
                    Some(memory.dedicated - memory.current_available),
                    Some(memory.dedicated),
                )
            } else {
                (None, None)
            };
            let power = self
                .nvml
                .as_mut()
                .and_then(|nvml| nvml.power_w(self.handles.len()).ok());
            let sample = project("nvapi", utilization, temperature, used, total, power, at);
            if !has_telemetry(&sample) {
                Err("NVAPI returned no supported telemetry metrics".into())
            } else {
                Ok(sample)
            }
        }
    }
}

impl Drop for Nvapi {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.unload)();
            let _ = FreeLibrary(self.module);
        }
    }
}

unsafe fn proc<T>(module: HMODULE, name: PCSTR) -> Result<T, String> {
    GetProcAddress(module, name)
        .map(|p| transmute_copy_ptr(p as *const () as *mut c_void))
        .ok_or_else(|| format!("missing export {}", name.display()))
}

unsafe fn nv_proc<T>(query: NvQuery, id: u32, name: &str) -> Result<T, String> {
    let p = query(id);
    if p.is_null() {
        Err(format!("NVAPI function {name} is unavailable"))
    } else {
        Ok(transmute_copy_ptr(p))
    }
}

unsafe fn transmute_copy_ptr<T>(p: *mut c_void) -> T {
    std::ptr::read((&p as *const *mut c_void).cast::<T>())
}

fn status(code: NvStatus, operation: &str) -> Result<(), String> {
    if code == NVAPI_OK {
        Ok(())
    } else {
        Err(format!("{operation} failed ({code})"))
    }
}

type AdlxResult = i32;
type AdlxInit = unsafe extern "C" fn(u64, *mut *mut AdlxSystem) -> AdlxResult;
type AdlxTerminate = unsafe extern "C" fn() -> AdlxResult;

#[repr(C)]
struct AdlxSystem {
    vtable: *const AdlxSystemVtable,
}

#[repr(C)]
struct AdlxSystemVtable {
    hybrid: usize,
    get_gpus: unsafe extern "system" fn(*mut AdlxSystem, *mut *mut AdlxGpuList) -> AdlxResult,
    query: usize,
    displays: usize,
    desktops: usize,
    changed: usize,
    log: usize,
    settings: usize,
    tuning: usize,
    performance:
        unsafe extern "system" fn(*mut AdlxSystem, *mut *mut AdlxPerformance) -> AdlxResult,
    ram: usize,
    i2c: usize,
}

#[repr(C)]
struct AdlxInterface {
    vtable: *const AdlxInterfaceVtable,
}

#[repr(C)]
struct AdlxInterfaceVtable {
    acquire: usize,
    release: unsafe extern "system" fn(*mut AdlxInterface) -> i32,
    query: usize,
}

#[repr(C)]
struct AdlxGpuList {
    vtable: *const AdlxGpuListVtable,
}

#[repr(C)]
struct AdlxGpuListVtable {
    base: AdlxInterfaceVtable,
    size: unsafe extern "system" fn(*mut AdlxGpuList) -> u32,
    empty: usize,
    begin: usize,
    end: usize,
    at_base: usize,
    clear: usize,
    remove_back: usize,
    add_back: usize,
    at: unsafe extern "system" fn(*mut AdlxGpuList, u32, *mut *mut AdlxGpu) -> AdlxResult,
    add_gpu: usize,
}

#[repr(C)]
struct AdlxGpu {
    vtable: *const AdlxGpuVtable,
}

#[repr(C)]
struct AdlxGpuVtable {
    base: AdlxInterfaceVtable,
    vendor: usize,
    family: usize,
    kind: usize,
    external: usize,
    name: unsafe extern "system" fn(*mut AdlxGpu, *mut *const c_char) -> AdlxResult,
    driver_path: usize,
    pnp: usize,
    desktops: usize,
    total_vram: unsafe extern "system" fn(*mut AdlxGpu, *mut u32) -> AdlxResult,
    vram_type: usize,
    bios: usize,
    device: usize,
    revision: usize,
    subsystem: usize,
    subsystem_vendor: usize,
    unique: usize,
}

#[repr(C)]
struct AdlxPerformance {
    vtable: *const AdlxPerformanceVtable,
}

#[repr(C)]
struct AdlxPerformanceVtable {
    base: AdlxInterfaceVtable,
    unused: [usize; 15],
    current_gpu: unsafe extern "system" fn(
        *mut AdlxPerformance,
        *mut AdlxGpu,
        *mut *mut AdlxMetrics,
    ) -> AdlxResult,
    current_system: usize,
    current_fps: usize,
    supported_gpu: usize,
    supported_system: usize,
}

#[repr(C)]
struct AdlxMetrics {
    vtable: *const AdlxMetricsVtable,
}

#[repr(C)]
struct AdlxMetricsVtable {
    base: AdlxInterfaceVtable,
    timestamp: usize,
    usage: unsafe extern "system" fn(*mut AdlxMetrics, *mut f64) -> AdlxResult,
    clock: usize,
    vram_clock: usize,
    temperature: unsafe extern "system" fn(*mut AdlxMetrics, *mut f64) -> AdlxResult,
    hotspot: usize,
    power: unsafe extern "system" fn(*mut AdlxMetrics, *mut f64) -> AdlxResult,
    board_power: unsafe extern "system" fn(*mut AdlxMetrics, *mut f64) -> AdlxResult,
    fan: usize,
    vram: unsafe extern "system" fn(*mut AdlxMetrics, *mut i32) -> AdlxResult,
    voltage: usize,
    intake: usize,
}

struct Adlx {
    module: HMODULE,
    terminate: AdlxTerminate,
    gpu: NonNull<AdlxGpu>,
    performance: NonNull<AdlxPerformance>,
}

impl Adlx {
    fn open() -> Result<Self, String> {
        unsafe {
            let module = LoadLibraryExW(w!("amdadlx64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
                .map_err(|e| format!("load trusted amdadlx64.dll: {e}"))?;
            let mut cleanup: Option<AdlxTerminate> = None;
            let result = (|| {
                let initialize: AdlxInit = proc(module, s!("ADLXInitialize"))?;
                let terminate: AdlxTerminate = proc(module, s!("ADLXTerminate"))?;
                let version = (1u64 << 48) | (5u64 << 32) | 124;
                let mut system = null_mut();
                adlx_status(initialize(version, &mut system), "ADLXInitialize")?;
                cleanup = Some(terminate);
                let system = NonNull::new(system).ok_or("ADLX returned a null system")?;
                let mut list = null_mut();
                adlx_status(
                    ((*(*system.as_ptr()).vtable).get_gpus)(system.as_ptr(), &mut list),
                    "ADLX GetGPUs",
                )?;
                let list = NonNull::new(list).ok_or("ADLX returned a null GPU list")?;
                let count = ((*(*list.as_ptr()).vtable).size)(list.as_ptr());
                if count == 0 || count > 64 {
                    release(list.as_ptr());
                    return Err(format!("ADLX returned invalid GPU count {count}"));
                }
                let mut gpu = null_mut();
                let at_result = ((*(*list.as_ptr()).vtable).at)(list.as_ptr(), 0, &mut gpu);
                release(list.as_ptr());
                adlx_status(at_result, "ADLX GPU list At")?;
                let gpu = NonNull::new(gpu).ok_or("ADLX returned a null GPU")?;
                let mut performance = null_mut();
                let perf_result =
                    ((*(*system.as_ptr()).vtable).performance)(system.as_ptr(), &mut performance);
                if let Err(e) = adlx_status(perf_result, "ADLX performance services") {
                    release(gpu.as_ptr());
                    return Err(e);
                }
                let performance = NonNull::new(performance).ok_or_else(|| {
                    release(gpu.as_ptr());
                    "ADLX returned null performance services"
                })?;
                Ok(Self {
                    module,
                    terminate,
                    gpu,
                    performance,
                })
            })();
            if result.is_err() {
                if let Some(terminate) = cleanup {
                    let _ = terminate();
                }
                let _ = FreeLibrary(module);
            }
            result
        }
    }

    fn sample(&mut self) -> Result<Sample, String> {
        unsafe {
            let mut metrics = null_mut();
            adlx_status(
                ((*(*self.performance.as_ptr()).vtable).current_gpu)(
                    self.performance.as_ptr(),
                    self.gpu.as_ptr(),
                    &mut metrics,
                ),
                "ADLX current GPU metrics",
            )?;
            let metrics = NonNull::new(metrics).ok_or("ADLX returned null GPU metrics")?;
            let vtable = &*(*metrics.as_ptr()).vtable;
            let mut usage = 0.0;
            let mut temperature = 0.0;
            let mut used_mb = 0i32;
            let mut power = 0.0;
            let usage = (vtable.usage)(metrics.as_ptr(), &mut usage)
                .eq(&0)
                .then_some(usage);
            let temperature = (vtable.temperature)(metrics.as_ptr(), &mut temperature)
                .eq(&0)
                .then_some(temperature);
            let used = (vtable.vram)(metrics.as_ptr(), &mut used_mb)
                .eq(&0)
                .then_some(used_mb)
                .filter(|v| *v >= 0)
                .map(|v| v as u64 * 1024 * 1024);
            let power = adlx_board_power((vtable.board_power)(metrics.as_ptr(), &mut power), power);
            release(metrics.as_ptr());

            let mut total_mb = 0u32;
            let total =
                ((*(*self.gpu.as_ptr()).vtable).total_vram)(self.gpu.as_ptr(), &mut total_mb)
                    .eq(&0)
                    .then_some(total_mb as u64 * 1024 * 1024);
            let sample = project(
                "adlx",
                usage,
                temperature,
                used,
                total,
                power,
                SystemTime::now(),
            );
            if !has_telemetry(&sample) {
                Err("ADLX returned no supported telemetry metrics".into())
            } else {
                Ok(sample)
            }
        }
    }
}

type NvmlReturn = u32;
type NvmlDevice = *mut c_void;
type NvmlInit = unsafe extern "C" fn() -> NvmlReturn;
type NvmlShutdown = unsafe extern "C" fn() -> NvmlReturn;
type NvmlCount = unsafe extern "C" fn(*mut u32) -> NvmlReturn;
type NvmlHandle = unsafe extern "C" fn(u32, *mut NvmlDevice) -> NvmlReturn;
type NvmlPower = unsafe extern "C" fn(NvmlDevice, *mut u32) -> NvmlReturn;

struct Nvml {
    module: HMODULE,
    shutdown: NvmlShutdown,
    count: NvmlCount,
    handle: NvmlHandle,
    power: NvmlPower,
}

impl Nvml {
    fn open() -> Result<Self, String> {
        unsafe {
            let module = load_nvml_module()?;
            let result = (|| {
                let init: NvmlInit = proc(module, s!("nvmlInit_v2"))?;
                let shutdown: NvmlShutdown = proc(module, s!("nvmlShutdown"))?;
                let count = proc(module, s!("nvmlDeviceGetCount_v2"))?;
                let handle = proc(module, s!("nvmlDeviceGetHandleByIndex_v2"))?;
                let power = proc(module, s!("nvmlDeviceGetPowerUsage"))?;
                if init() != 0 {
                    return Err("nvmlInit_v2 failed".into());
                }
                Ok(Self {
                    module,
                    shutdown,
                    count,
                    handle,
                    power,
                })
            })();
            if result.is_err() {
                let _ = FreeLibrary(module);
            }
            result
        }
    }

    fn power_w(&mut self, nvapi_count: usize) -> Result<f64, String> {
        unsafe {
            let mut count = 0;
            if (self.count)(&mut count) != 0 || count != 1 || nvapi_count != 1 {
                return Err("NVML/NVAPI PCI alignment unavailable for multiple GPUs".into());
            }
            let mut device = null_mut();
            if (self.handle)(0, &mut device) != 0 || device.is_null() {
                return Err("NVML device unavailable".into());
            }
            let mut milliwatts = 0;
            if (self.power)(device, &mut milliwatts) != 0 {
                return Err("NVML power usage unavailable".into());
            }
            Ok(milliwatts as f64 / 1000.0)
        }
    }
}

unsafe fn load_nvml_module() -> Result<HMODULE, String> {
    if let Ok(module) = LoadLibraryExW(w!("nvml.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) {
        return Ok(module);
    }
    let program_files = SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, None)
        .map_err(|e| format!("resolve Program Files for legacy NVML: {e}"))?;
    let root = program_files.to_string();
    CoTaskMemFree(Some(program_files.0.cast()));
    let path = std::path::PathBuf::from(
        root.map_err(|e| format!("decode Program Files path for legacy NVML: {e}"))?,
    )
    .join("NVIDIA Corporation")
    .join("NVSMI")
    .join("nvml.dll");
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    LoadLibraryExW(
        PCWSTR(wide.as_ptr()),
        None,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
    )
    .map_err(|e| format!("load trusted legacy NVML at {}: {e}", path.display()))
}

impl Drop for Nvml {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.shutdown)();
            let _ = FreeLibrary(self.module);
        }
    }
}

impl Drop for Adlx {
    fn drop(&mut self) {
        unsafe {
            release(self.performance.as_ptr());
            release(self.gpu.as_ptr());
            let _ = (self.terminate)();
            let _ = FreeLibrary(self.module);
        }
    }
}

unsafe fn release<T>(value: *mut T) {
    let interface = value.cast::<AdlxInterface>();
    ((*(*interface).vtable).release)(interface);
}

fn adlx_status(code: AdlxResult, operation: &str) -> Result<(), String> {
    if matches!(code, 0..=2) {
        Ok(())
    } else {
        Err(format!("{operation} failed ({code})"))
    }
}

fn has_telemetry(sample: &Sample) -> bool {
    !sample.readings.is_empty() || sample.temperature.valid || sample.power_w.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitration_is_native_only_and_ordered() {
        assert_eq!(candidates("auto"), &["nvapi", "adlx"]);
        assert_eq!(candidates("nvapi"), &["nvapi"]);
        assert_eq!(candidates("adlx"), &["adlx"]);
        assert!(candidates("off").is_empty());
    }

    #[test]
    fn adlx_accepts_only_success_equivalent_results() {
        for code in [0, 1, 2] {
            assert!(adlx_status(code, "test").is_ok());
        }
        assert!(adlx_status(3, "test").is_err());
        assert!(adlx_status(-1, "test").is_err());
    }

    #[test]
    fn versioned_structs_match_vendor_abi() {
        assert_eq!(size_of::<NvDynamicPstates>(), 72);
        assert_eq!(size_of::<NvThermalSettings>(), 68);
        assert_eq!(size_of::<NvMemoryInfo>(), 80);
        assert_eq!(nv_version::<NvDynamicPstates>(1), 0x10048);
        assert_eq!(nv_version::<NvThermalSettings>(2), 0x20044);
        assert_eq!(nv_version::<NvMemoryInfo>(1), 0x10050);
    }

    #[test]
    fn projection_validates_and_derives_vram_percent() {
        let sample = project(
            "test",
            Some(42.0),
            Some(70.0),
            Some(3),
            Some(4),
            Some(250.0),
            SystemTime::UNIX_EPOCH,
        );
        assert_eq!(sample.readings.len(), 4);
        assert_eq!(
            sample
                .readings
                .iter()
                .find(|r| r.key == MetricKey::VRAMUtilization)
                .unwrap()
                .number,
            75.0
        );
        let invalid = project(
            "test",
            Some(f64::NAN),
            Some(500.0),
            Some(5),
            Some(4),
            Some(f64::INFINITY),
            SystemTime::UNIX_EPOCH,
        );
        assert!(invalid.readings.iter().all(|r| {
            r.key != MetricKey::GPUUtilization && r.key != MetricKey::VRAMUtilization
        }));
        assert!(!invalid.temperature.valid);
        assert!(invalid.power_w.is_none());
    }

    #[test]
    fn adlx_power_only_sample_is_usable() {
        let sample = project(
            "adlx",
            None,
            None,
            None,
            None,
            Some(200.0),
            SystemTime::UNIX_EPOCH,
        );
        assert!(has_telemetry(&sample));
        assert_eq!(adlx_board_power(0, 200.0), Some(200.0));
        assert_eq!(adlx_board_power(1, 200.0), None);
    }

    #[test]
    fn nvml_power_only_sample_is_usable() {
        let sample = project(
            "nvapi",
            None,
            None,
            None,
            None,
            Some(175.5),
            SystemTime::UNIX_EPOCH,
        );
        assert!(has_telemetry(&sample));
    }

    #[test]
    #[ignore = "requires an installed NVIDIA or AMD display driver"]
    fn live_native_smoke() {
        let mut provider = Provider::new("auto");
        match provider.sample("auto") {
            Ok(sample) => println!(
                "backend={} metrics={} temperature={:?}",
                sample.backend,
                sample.readings.len(),
                sample.temperature.valid.then_some(sample.temperature.value)
            ),
            Err(e) => println!("native GPU unavailable: {e}"),
        }
    }
}
