//! Reads Core Audio endpoints and records shared-stream results without starting playback.

use std::{fmt::Write, ptr};
use windows::{
    core::{Error, GUID, HRESULT},
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Media::{
            Audio::{
                eCommunications, eConsole, eMultimedia, eRender, IAudioClient, IMMDevice,
                IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
                WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
            },
            KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
            StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
            CLSCTX_ALL, COINIT_APARTMENTTHREADED, STGM_READ,
        },
    },
};

/// Collects local endpoint evidence; the parent process owns the overall time limit.
pub(super) fn collect() -> String {
    let mut report = String::from(
        "Windows output diagnostic v2\nIndependent Core Audio checks; these results do not capture Unity's own audio-client calls.\nShared-mode streams are initialized and released without starting playback.\n",
    );
    // SAFETY: This is a fresh helper thread with no existing COM users. All COM references are
    // confined to collect_endpoints and dropped before the matching CoUninitialize call.
    unsafe {
        let status = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if status.is_err() {
            let _ = writeln!(report, "COM initialization: {}", status_text(status));
            return report;
        }
        if let Err(error) = collect_endpoints(&mut report) {
            let _ = writeln!(
                report,
                "Endpoint enumeration: {}",
                status_text(error.code())
            );
        }
        CoUninitialize();
    }
    report
}

/// Owns endpoint references while the helper's COM apartment is initialized.
unsafe fn collect_endpoints(report: &mut String) -> windows::core::Result<()> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    for (label, role) in [
        ("console", eConsole),
        ("multimedia", eMultimedia),
        ("communications", eCommunications),
    ] {
        match unsafe { enumerator.GetDefaultAudioEndpoint(eRender, role) } {
            Ok(device) => {
                let _ = writeln!(report, "Default {label}: {}", unsafe {
                    device_label(&device)
                });
            }
            Err(error) => {
                let _ = writeln!(report, "Default {label}: {}", status_text(error.code()));
            }
        }
    }
    let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)? };
    let count = unsafe { devices.GetCount()? };
    let _ = writeln!(report, "Active output endpoints: {count}");
    for index in 0..count {
        match unsafe { devices.Item(index) } {
            Ok(device) => {
                let _ = writeln!(report, "Endpoint {index}: {}", unsafe {
                    device_label(&device)
                });
                if let Err(error) = unsafe { inspect_formats(&device, report) } {
                    let _ = writeln!(report, "  Audio client: {}", status_text(error.code()));
                }
            }
            Err(error) => {
                let _ = writeln!(report, "Endpoint {index}: {}", status_text(error.code()));
            }
        }
    }
    Ok(())
}

/// Reads a quoted friendly name and opaque endpoint ID, retaining errors from either lookup.
unsafe fn device_label(device: &IMMDevice) -> String {
    let name = unsafe { friendly_name(device) }
        .unwrap_or_else(|error| format!("name unavailable: {}", status_text(error.code())));
    let id = match unsafe { device.GetId() } {
        Ok(value) => {
            let _memory = TaskMemory(value.0);
            unsafe { value.to_string() }
                .unwrap_or_else(|error| format!("ID text unavailable: {error}"))
        }
        Err(error) => format!("ID unavailable: {}", status_text(error.code())),
    };
    format!("{name:?} ({id:?})")
}

/// Copies one property-store string and releases both native allocations on all paths.
unsafe fn friendly_name(device: &IMMDevice) -> windows::core::Result<String> {
    let properties = unsafe { device.OpenPropertyStore(STGM_READ)? };
    let mut value = unsafe { properties.GetValue(&PKEY_Device_FriendlyName)? };
    let text = unsafe { PropVariantToStringAlloc(&value) };
    let _ = unsafe { PropVariantClear(&mut value) };
    let text = text?;
    let _memory = TaskMemory(text.0);
    Ok(unsafe { text.to_string() }?)
}

/// Separates channel count, sample representation and callback mode on each endpoint.
unsafe fn inspect_formats(device: &IMMDevice, report: &mut String) -> windows::core::Result<()> {
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    match unsafe { client.GetMixFormat() } {
        Ok(pointer) if !pointer.is_null() => {
            let _memory = TaskMemory(pointer);
            let _ = writeln!(report, "  Native mix: {}", unsafe {
                describe_format(pointer)
            });
            let status = unsafe { initialize_client(&client, pointer, 0) };
            let _ = writeln!(
                report,
                "  Initialize native mix on original client: {}",
                status_text(status)
            );
            // Release the original stream before opening independent clients, keeping the
            // comparison free of an extra concurrent initialized stream on this endpoint.
            drop(client);
            unsafe { inspect_format(device, pointer, "native mix", report)? };
        }
        Ok(_) => {
            let _ = writeln!(report, "  GetMixFormat returned a null format");
        }
        Err(error) => {
            let _ = writeln!(report, "  GetMixFormat: {}", status_text(error.code()));
        }
    }
    for rate in [48_000, 44_100] {
        let standard = stereo_pcm(rate);
        unsafe {
            inspect_format(
                device,
                &standard,
                &format!("{rate} Hz PCM, 16-bit stereo"),
                report,
            )?;
        }
        // The 48 kHz extensible PCM16 stereo control is included in the matrix below.
        if rate == 44_100 {
            let format = extensible_format(rate, 2, 16, KSDATAFORMAT_SUBTYPE_PCM);
            unsafe {
                inspect_format(
                    device,
                    ptr::addr_of!(format.Format),
                    "44100 Hz extensible PCM, 16-bit stereo",
                    report,
                )?;
            }
        }
    }
    for channels in [2, 8] {
        for (encoding, bits, subtype) in [
            (
                "float32",
                32,
                GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71),
            ),
            ("PCM32", 32, KSDATAFORMAT_SUBTYPE_PCM),
            ("PCM16", 16, KSDATAFORMAT_SUBTYPE_PCM),
        ] {
            let format = extensible_format(48_000, channels, bits, subtype);
            unsafe {
                inspect_format(
                    device,
                    ptr::addr_of!(format.Format),
                    &format!("48000 Hz, {channels} channels, {encoding}, extensible"),
                    report,
                )?;
            }
        }
    }
    Ok(())
}

/// Checks support and actual opens on fresh clients, including any suggested closest format.
unsafe fn inspect_format(
    device: &IMMDevice,
    format: *const WAVEFORMATEX,
    label: &str,
    report: &mut String,
) -> windows::core::Result<()> {
    let _ = writeln!(report, "  Format {label}: {}", unsafe {
        describe_format(format)
    });
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let mut closest = ptr::null_mut();
    let status =
        unsafe { client.IsFormatSupported(AUDCLNT_SHAREMODE_SHARED, format, Some(&mut closest)) };
    let _memory = TaskMemory(closest);
    let _ = writeln!(report, "  Support {label}: {}", status_text(status));
    if !closest.is_null() {
        let _ = writeln!(report, "    Closest: {}", unsafe {
            describe_format(closest)
        });
        unsafe { initialize_shared(device, closest, "closest alternative", 0, report) };
    }
    drop(client);
    for flags in [0, AUDCLNT_STREAMFLAGS_EVENTCALLBACK] {
        unsafe { initialize_shared(device, format, label, flags, report) };
    }
    Ok(())
}

/// Opens a temporary shared stream with a 100 ms buffer, without ever calling Start.
unsafe fn initialize_shared(
    device: &IMMDevice,
    format: *const WAVEFORMATEX,
    label: &str,
    flags: u32,
    report: &mut String,
) {
    let status = match unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) } {
        Ok(client) => unsafe { initialize_client(&client, format, flags) },
        Err(error) => error.code(),
    };
    let _ = writeln!(
        report,
        "  Initialize shared {label}, flags=0x{flags:08X}: {}",
        status_text(status)
    );
}

/// Borrows one live format for synchronous initialization and preserves its exact HRESULT.
unsafe fn initialize_client(
    client: &IAudioClient,
    format: *const WAVEFORMATEX,
    flags: u32,
) -> HRESULT {
    // SAFETY: The caller owns the complete format allocation for this synchronous call. The
    // client is initialized at most once and never started, including event-driven clients.
    match unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, format, None) }
    {
        Ok(()) => HRESULT(0),
        Err(error) => error.code(),
    }
}

/// Builds the same extensible container for the channel and sample representation comparisons.
fn extensible_format(rate: u32, channels: u16, bits: u16, subtype: GUID) -> WAVEFORMATEXTENSIBLE {
    let alignment = channels * bits / 8;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: 0xfffe,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * u32::from(alignment),
            nBlockAlign: alignment,
            wBitsPerSample: bits,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: bits,
        },
        dwChannelMask: if channels == 2 { 3 } else { 0x63f },
        SubFormat: subtype,
    }
}

/// Builds the standard 16-bit stereo representation, without changing endpoint settings.
fn stereo_pcm(rate: u32) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 1,
        nChannels: 2,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * 4,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Copies the common and extensible format fields while the caller owns their native allocation.
unsafe fn describe_format(pointer: *const WAVEFORMATEX) -> String {
    let format = unsafe { pointer.read_unaligned() };
    let (rate, channels, bits, tag) = (
        format.nSamplesPerSec,
        format.nChannels,
        format.wBitsPerSample,
        format.wFormatTag,
    );
    let (alignment, byte_rate, extra) = (format.nBlockAlign, format.nAvgBytesPerSec, format.cbSize);
    let mut description = format!(
        "{rate} Hz, {channels} channels, {bits} bits, format tag 0x{tag:04X}, block align {alignment}, bytes/sec {byte_rate}, extra bytes {extra}"
    );
    if tag == 0xfffe
        && usize::from(format.cbSize)
            >= size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()
    {
        // SAFETY: Core Audio owns the declared cbSize bytes after WAVEFORMATEX. Only read the
        // extension when its declared length contains the complete extensible structure.
        let extension = unsafe { pointer.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
        let valid_bits = unsafe { extension.Samples.wValidBitsPerSample };
        let (mask, subtype) = (extension.dwChannelMask, extension.SubFormat);
        let _ = write!(
            description,
            ", {valid_bits} valid bits, channel mask 0x{mask:08X}, subtype {subtype:?}"
        );
    }
    // SAFETY: The caller owns the format and all cbSize extension bytes, whether allocated by
    // Core Audio or constructed above. Record those exact bytes without changing them.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            pointer.cast::<u8>(),
            size_of::<WAVEFORMATEX>() + usize::from(extra),
        )
    };
    description.push_str(", bytes=");
    for byte in bytes {
        let _ = write!(description, "{byte:02X}");
    }
    description
}

/// Retains the raw HRESULT rather than interpreting FMOD's generic message as a diagnosis.
fn status_text(status: HRESULT) -> String {
    let description = match status.0 {
        0 => "succeeded".to_string(),
        1 => "closest format; not an exact match".to_string(),
        _ => Error::from_hresult(status).to_string(),
    };
    format!("0x{:08X} ({description})", status.0 as u32)
}

/// Releases a COM-task allocation after its borrowed value has been copied or inspected.
struct TaskMemory<T>(*mut T);

impl<T> Drop for TaskMemory<T> {
    fn drop(&mut self) {
        // SAFETY: Each instance uniquely owns a CoTaskMem allocation returned by Core Audio.
        // The allocator explicitly permits a null pointer.
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_payloads_match_the_captured_windows_format_bytes() {
        // These are the six complete WAVEFORMATEXTENSIBLE payloads recorded by the native
        // Windows control. Keep representation and channel layout independent in the probe.
        for (channels, bits, float, expected) in [
            (
                2,
                32,
                true,
                "FEFF020080BB000000DC05000800200016002000030000000300000000001000800000AA00389B71",
            ),
            (
                2,
                32,
                false,
                "FEFF020080BB000000DC05000800200016002000030000000100000000001000800000AA00389B71",
            ),
            (
                2,
                16,
                false,
                "FEFF020080BB000000EE02000400100016001000030000000100000000001000800000AA00389B71",
            ),
            (
                8,
                32,
                true,
                "FEFF080080BB00000070170020002000160020003F0600000300000000001000800000AA00389B71",
            ),
            (
                8,
                32,
                false,
                "FEFF080080BB00000070170020002000160020003F0600000100000000001000800000AA00389B71",
            ),
            (
                8,
                16,
                false,
                "FEFF080080BB000000B80B0010001000160010003F0600000100000000001000800000AA00389B71",
            ),
        ] {
            let subtype = if float {
                GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71)
            } else {
                KSDATAFORMAT_SUBTYPE_PCM
            };
            let format = extensible_format(48_000, channels, bits, subtype);
            let description = unsafe { describe_format(ptr::addr_of!(format.Format)) };
            assert!(
                description.ends_with(&format!("bytes={expected}")),
                "{description}"
            );
        }
    }
}
