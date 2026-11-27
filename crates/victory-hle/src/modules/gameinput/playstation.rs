//! PlayStation pads read straight from HID: the PS4's DualShock 4 (both
//! revisions and Sony's wireless adapter) and the PS5's DualSense and
//! DualSense Edge. They are not XInput devices, so without a remapper (Steam
//! Input, DS4Windows) XInput never lists them. Reports can be USB or
//! Bluetooth. A scan thread looks for new pads every [`SCAN`]; each pad has a
//! reader thread that keeps its latest state until the pad goes away, and
//! sends the pad the motor speeds the title last asked for. By default XInput
//! has priority over HID, since a remapper can present the same pad twice.
//!
//! The DualShock 4 reading and the rumble reports (both pads, with the CRC
//! that Bluetooth output reports carry) are ported from
//! FrancescoGelosoEterea/VIscosity, commit d8db7a0 (`gameinput/hid.rs`, by
//! marquezini), GPL-3.0 like this crate. Its report layouts are the ones
//! Linux's `hid-playstation` and `hid-sony` drivers read and write. Its
//! generic HID pads are not ported: this reader opens Sony's pads only.

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

const SONY: u16 = 0x054C;
/// The DualShock 4 wireless adapter: a USB device that sends the USB
/// reports of the pad paired with it, and reports while no pad is.
const DONGLE: u16 = 0x0BA0;

/// The pads we read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sony {
    DualShock4,
    DualSense,
}

impl Sony {
    /// The pad with this vendor and product id, `None` for anything else.
    pub(super) fn of(vendor: u16, product: u16) -> Option<Sony> {
        match (vendor, product) {
            // Both revisions, and the wireless adapter.
            (SONY, 0x05C4 | 0x09CC | DONGLE) => Some(Sony::DualShock4),
            // DualSense and DualSense Edge.
            (SONY, 0x0CE6 | 0x0DF2) => Some(Sony::DualSense),
            _ => None,
        }
    }

    /// The calibration feature report: asking for it switches a pad on
    /// Bluetooth from its short reports to the full ones.
    #[cfg(windows)]
    fn full_reports_feature(self) -> u8 {
        match self {
            Sony::DualShock4 => 0x02,
            Sony::DualSense => 0x05,
        }
    }

    /// The feature report with the pad's Bluetooth address at 1..7: the
    /// DualSense's on both transports, the DualShock 4's over USB only.
    #[cfg(windows)]
    fn pairing_feature(self) -> u8 {
        match self {
            Sony::DualShock4 => 0x12,
            Sony::DualSense => 0x09,
        }
    }

    #[cfg(windows)]
    fn name(self) -> &'static str {
        match self {
            Sony::DualShock4 => "DualShock 4",
            Sony::DualSense => "DualSense",
        }
    }
}

/// The D-pad hat (low nibble of the first button byte): 0 up, clockwise in
/// eighths, 8 and above centred.
fn hat(value: u8) -> u32 {
    match value & 0xF {
        0 => UP,
        1 => UP | RIGHT,
        2 => RIGHT,
        3 => RIGHT | DOWN,
        4 => DOWN,
        5 => DOWN | LEFT,
        6 => LEFT,
        7 => LEFT | UP,
        _ => 0,
    }
}

/// Face buttons in the first button byte, the rest in the second and third
/// (the PS button and the mute button have no console pad button). Both pads
/// lay the three bytes out alike; the DualShock 4's Share is the
/// DualSense's Create.
const BUTTONS: [(usize, u8, u32); 11] = [
    (0, 0x10, X), // square
    (0, 0x20, A), // cross
    (0, 0x40, B), // circle
    (0, 0x80, Y), // triangle
    (1, 0x01, LEFT_SHOULDER),
    (1, 0x02, RIGHT_SHOULDER),
    (1, 0x10, VIEW), // create
    (1, 0x20, MENU), // options
    (1, 0x40, LEFT_STICK),
    (1, 0x80, RIGHT_STICK),
    (2, 0x02, VIEW), // touchpad click
];

/// The pad in one input report, `None` for a report of another kind.
/// `bluetooth`: the pad is connected over Bluetooth, where the DualSense's
/// full report is 0x31 (one byte further in) and 0x01 is the short report
/// the pad sends before it was asked for the full one. The DualShock 4's
/// 0x01 is the same report over USB and Bluetooth (buttons before the
/// triggers, as in the DualSense's short one), and its full Bluetooth report
/// 0x11 is that two bytes further in; 0x11 is never a USB report, so it
/// needs no transport.
pub(super) fn parse(pad: Sony, report: &[u8], bluetooth: bool) -> Option<PadState> {
    // Offsets of the sticks (LX LY RX RY), the triggers (L2 R2) and the
    // three button bytes.
    let (sticks, triggers, buttons) = match (pad, report.first()?, bluetooth) {
        (Sony::DualSense, 0x01, false) => (1, 5, 8),
        (Sony::DualSense, 0x31, true) => (2, 6, 9),
        (Sony::DualSense, 0x01, true) => (1, 8, 5),
        (Sony::DualShock4, 0x01, _) => (1, 8, 5),
        (Sony::DualShock4, 0x11, _) => (3, 10, 7),
        _ => return None,
    };
    if report.len() < sticks + 4 || report.len() < triggers + 2 || report.len() < buttons + 3 {
        return None;
    }
    let bytes = &report[buttons..buttons + 3];
    // 0 to 255 with 128 centred; down is positive on the pad, up here.
    let axis = |v: u8| ((f32::from(v) - 127.5) / 127.5).clamp(-1.0, 1.0);
    Some(PadState {
        buttons: BUTTONS
            .iter()
            .filter(|&&(byte, mask, _)| bytes[byte] & mask != 0)
            .fold(hat(bytes[0]), |all, &(_, _, bit)| all | bit),
        left_trigger: f32::from(report[triggers]) / 255.0,
        right_trigger: f32::from(report[triggers + 1]) / 255.0,
        left_x: axis(report[sticks]),
        left_y: -axis(report[sticks + 1]),
        right_x: axis(report[sticks + 2]),
        right_y: -axis(report[sticks + 3]),
    })
}

/// The Bluetooth address in a pairing feature report (0x09, the address at
/// 1..7 on both transports), `None` when it is too short or all zeros.
fn mac(pairing: &[u8]) -> Option<[u8; 6]> {
    let mac: [u8; 6] = pairing.get(1..7)?.try_into().ok()?;
    (mac != [0; 6]).then_some(mac)
}

/// The Bluetooth address in the serial number Windows gives a Bluetooth HID
/// device (twelve hex digits, the first byte first as the address is
/// written; separators allowed), in the pairing report's order (last byte
/// first), `None` for anything else. A DualShock 4 on Bluetooth has no
/// pairing report to ask, and without the address its USB twin is not
/// recognised: only that is lost when this reads nothing.
fn serial_mac(serial: &str) -> Option<[u8; 6]> {
    let digits: Vec<u8> = serial
        .bytes()
        .filter(|c| !matches!(c, b':' | b'-'))
        .map(|c| char::from(c).to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()?;
    if digits.len() != 12 {
        return None;
    }
    let mut mac = [0u8; 6];
    // Last byte first.
    for (byte, pair) in mac.iter_mut().rev().zip(digits.chunks(2)) {
        *byte = pair[0] << 4 | pair[1];
    }
    (mac != [0; 6]).then_some(mac)
}

/// The DualShock 4 adapter's report while no pad is paired with it: bit 2
/// of byte 31 of its 64-byte USB report, which a pad (wired or through the
/// adapter) leaves clear (as Linux's `hid-sony` reads it).
fn dongle_empty(report: &[u8]) -> bool {
    report.len() >= 64 && report[0] == 0x01 && report[31] & 0x04 != 0
}

/// CRC-32 (IEEE) as Sony's Bluetooth output reports carry it.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for byte in parts.iter().flat_map(|p| p.iter()) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// The output report that sets the motors (`low` the left, heavy one;
/// `high` the right, light one); lights and the DualSense's adaptive
/// triggers are left as they are. Over Bluetooth the report is 78 bytes
/// ending in its CRC (seeded with 0xA2, the HID output header). Windows takes
/// a write as long as the device's longest output report (`length`, 547 bytes
/// for these pads over Bluetooth), so the report is padded with zeros to
/// that after the CRC, which stays at 74.
pub(super) fn rumble_report(
    pad: Sony,
    bluetooth: bool,
    length: usize,
    low: u16,
    high: u16,
) -> Vec<u8> {
    let (low, high) = ((low >> 8) as u8, (high >> 8) as u8);
    let mut r = vec![0u8; if bluetooth { 78 } else { length.max(32) }];
    // Where the part both transports share (flags, then the motors) starts.
    let common = match (pad, bluetooth) {
        (Sony::DualSense, false) => {
            r[0] = 0x02;
            1
        }
        (Sony::DualSense, true) => {
            r[0] = 0x31;
            r[1] = 0x00; // sequence 0
            r[2] = 0x10; // the tag Sony's driver sends
            3
        }
        (Sony::DualShock4, false) => {
            r[0] = 0x05;
            1
        }
        (Sony::DualShock4, true) => {
            r[0] = 0x11;
            r[1] = 0xC0; // a HID report, with a CRC
            3
        }
    };
    // DualSense: compatible vibration and haptics select. DualShock 4: the
    // motors only (not the light bar).
    r[common] = match pad {
        Sony::DualSense => 0x03,
        Sony::DualShock4 => 0x01,
    };
    let motors = match pad {
        Sony::DualSense => common + 2,
        Sony::DualShock4 => common + 3,
    };
    r[motors] = high;
    r[motors + 1] = low;
    if bluetooth {
        let crc = crc32(&[&[0xA2], &r[..74]]);
        r[74..].copy_from_slice(&crc.to_le_bytes());
        r.resize(length.max(78), 0);
    }
    r
}

#[cfg(windows)]
pub(super) use host::{pads, rumble, stop_rumble};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Off,
    Auto,
    Both,
}

/// `VICTORY_DUALSENSE`, named before the DualShock 4 was read and kept for
/// both: 0 reads no PlayStation pad, 1 reads them beside XInput's pads, and
/// anything else only while XInput lists none.
#[cfg(windows)]
pub(super) fn mode() -> Mode {
    static MODE: std::sync::LazyLock<Mode> =
        std::sync::LazyLock::new(|| match std::env::var("VICTORY_DUALSENSE").as_deref() {
            Ok("0") => Mode::Off,
            Ok("1") => Mode::Both,
            _ => Mode::Auto,
        });
    *MODE
}

/// XInput does not identify the corresponding physical HID device. Prefer
/// it in automatic mode. If both sources are requested, retain the slot of
/// every connected HID pad unless that slot has been claimed by XInput.
pub(super) fn slots(
    mode: Mode,
    mut xinput: [Option<PadState>; super::PADS],
    assigned: &mut [Option<u64>; super::PADS],
    hid: impl FnOnce() -> Vec<(u64, PadState)>,
) -> [Option<PadState>; super::PADS] {
    if mode == Mode::Off || mode == Mode::Auto && xinput.iter().any(Option::is_some) {
        assigned.fill(None);
        return xinput;
    }
    let pads = hid();
    for (slot, owner) in xinput.iter_mut().zip(assigned.iter_mut()) {
        if slot.is_none()
            && let Some((_, state)) = pads.iter().find(|(id, _)| Some(*id) == *owner)
        {
            *slot = Some(*state);
        } else {
            *owner = None;
        }
    }
    for (id, state) in pads {
        if assigned.contains(&Some(id)) {
            continue;
        }
        let Some(slot) = xinput.iter().position(Option::is_none) else {
            break;
        };
        xinput[slot] = Some(state);
        assigned[slot] = Some(id);
    }
    xinput
}

#[cfg(windows)]
mod host {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{LazyLock, Mutex, PoisonError};
    use std::time::Duration;

    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
        SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList,
        SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    };
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData,
        HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid, HidD_GetPreparsedData,
        HidD_GetSerialNumberString, HidP_GetCaps,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::IO::{
        CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
    };
    use windows_sys::Win32::System::Threading::CreateEventW;

    use super::{
        DONGLE, PadState, SONY, Sony, dongle_empty, mac, parse, rumble_report, serial_mac,
    };

    /// How often to look for newly connected pads.
    const SCAN: Duration = Duration::from_secs(2);

    /// A pad in full-report mode sends reports all the time (hundreds a
    /// second); this long without one and we ask it whether it is still
    /// there. Over Bluetooth Windows can keep the device after the pad is
    /// switched off or out of range, and a read then waits for ever.
    const QUIET_MS: u32 = 500;

    /// The longest a reader waits for a rumble report to go out before it
    /// gives up on it and reads on.
    const WRITE_MS: u32 = 100;

    struct Pad {
        /// This connection's: a pad that reconnects gets a new one.
        id: u64,
        path: Vec<u16>,
        /// The pad's Bluetooth address, the same over USB and Bluetooth:
        /// one pad seen through both is opened once.
        mac: Option<[u8; 6]>,
        /// `None` until the first report, and while the pad is silent.
        state: Option<PadState>,
        /// The motor speeds (low, high) the title asked for last; the
        /// reader sends them when they change.
        rumble: (u16, u16),
    }

    /// The open pads, in the order they were found.
    static PADS: Mutex<Vec<Pad>> = Mutex::new(Vec::new());

    fn lock() -> std::sync::MutexGuard<'static, Vec<Pad>> {
        PADS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Paths found to be a pad already open over its other transport, with
    /// its address: not opened again on every scan (two feature reports
    /// and a log line each time) while that pad stays open.
    static TWINS: Mutex<Vec<(Vec<u16>, [u8; 6])>> = Mutex::new(Vec::new());

    /// The open pads' latest states with their connection ids, oldest
    /// connection first, without the pads that are silent or yet to report;
    /// the first call starts looking for them.
    pub(in super::super) fn pads() -> Vec<(u64, PadState)> {
        static STARTED: LazyLock<()> = LazyLock::new(|| {
            std::thread::spawn(|| {
                loop {
                    scan();
                    std::thread::sleep(SCAN);
                }
            });
        });
        LazyLock::force(&STARTED);
        lock()
            .iter()
            .filter_map(|pad| Some((pad.id, pad.state?)))
            .collect()
    }

    /// Asks pad `id` (a connection id from [`pads`]) to run its motors at
    /// these speeds; its reader sends them with the next report. A pad that
    /// went away is ignored.
    pub(in super::super) fn rumble(id: u64, low: u16, high: u16) {
        if let Some(pad) = lock().iter_mut().find(|pad| pad.id == id) {
            pad.rumble = (low, high);
        }
    }

    /// Asks every open pad to stop its motors (the game is ending: a pad
    /// left rumbling would run on until it is switched off). Their readers
    /// send it with their next report, within a few milliseconds.
    pub(in super::super) fn stop_rumble() -> bool {
        let mut pads = lock();
        let any = pads.iter().any(|pad| pad.rumble != (0, 0));
        for pad in pads.iter_mut() {
            pad.rumble = (0, 0);
        }
        any
    }

    /// Windows sizes each report buffer for its top-level HID collection.
    /// The calibration and pairing reports use the same feature buffer; an
    /// output report is written as long as `output`, 0 when the pad
    /// declares none (then it does not rumble).
    struct Reports {
        input: Vec<u8>,
        feature: Vec<u8>,
        output: usize,
    }

    impl Reports {
        fn for_device(handle: HANDLE) -> Option<Self> {
            let mut data = 0;
            // SAFETY: the device is open and the output is writable.
            if !unsafe { HidD_GetPreparsedData(handle, &mut data) } {
                return None;
            }
            let mut caps = HIDP_CAPS::default();
            // SAFETY: `data` was obtained above and is released after its last use.
            let status = unsafe { HidP_GetCaps(data, &mut caps) };
            unsafe { HidD_FreePreparsedData(data) };
            if status != HIDP_STATUS_SUCCESS
                || caps.InputReportByteLength == 0
                || caps.FeatureReportByteLength == 0
            {
                return None;
            }
            Some(Self {
                input: vec![0; usize::from(caps.InputReportByteLength)],
                feature: vec![0; usize::from(caps.FeatureReportByteLength)],
                output: usize::from(caps.OutputReportByteLength),
            })
        }
    }

    /// Feature report `id` into `buffer` (its first byte), `false` when the
    /// pad did not answer. A round trip to the pad, so over Bluetooth it also
    /// tells whether the pad is still there.
    fn feature(handle: HANDLE, id: u8, buffer: &mut [u8]) -> bool {
        buffer.fill(0);
        buffer[0] = id;
        // SAFETY: the buffer is as long as we say.
        unsafe { HidD_GetFeature(handle, buffer.as_mut_ptr().cast(), buffer.len() as u32) }
    }

    /// Asks for the calibration report: it switches a pad on Bluetooth to
    /// full reports (DualSense 0x31, DualShock 4 0x11), again after it
    /// reconnected; over USB it changes nothing. `false` when the pad did
    /// not answer.
    fn full_reports(handle: HANDLE, pad: Sony, buffer: &mut [u8]) -> bool {
        feature(handle, pad.full_reports_feature(), buffer)
    }

    /// The pad's Bluetooth address, `None` when it does not tell: from its
    /// pairing report, or for a DualShock 4 on Bluetooth (no pairing report
    /// there) from the serial number Windows gives the device.
    fn address(handle: HANDLE, pad: Sony, bluetooth: bool, buffer: &mut [u8]) -> Option<[u8; 6]> {
        if pad == Sony::DualShock4 && bluetooth {
            let mut serial = [0u16; 128];
            // SAFETY: the buffer is as long as we say, in bytes.
            let ok = unsafe {
                HidD_GetSerialNumberString(
                    handle,
                    serial.as_mut_ptr().cast(),
                    size_of_val(&serial) as u32,
                )
            };
            let end = serial.iter().position(|&c| c == 0).unwrap_or(serial.len());
            return ok
                .then(|| serial_mac(&String::from_utf16_lossy(&serial[..end])))
                .flatten();
        }
        feature(handle, pad.pairing_feature(), buffer)
            .then(|| mac(buffer))
            .flatten()
    }

    /// Writes one output report, waiting at most [`WRITE_MS`] for it;
    /// `false` when it did not go out (the reader then notices a pad that
    /// went away on its own).
    fn write(handle: HANDLE, event: HANDLE, report: &[u8]) -> bool {
        // SAFETY: plain data; only the event is set.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;
        let mut put = 0;
        // SAFETY: the report and `overlapped` outlive the write: every way
        // out below waits for it to end first.
        let started = unsafe {
            WriteFile(
                handle,
                report.as_ptr(),
                report.len() as u32,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        };
        // SAFETY: plain query.
        if started == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return false;
        }
        // SAFETY: the write started above.
        if unsafe { GetOverlappedResultEx(handle, &overlapped, &mut put, WRITE_MS, 0) } != 0 {
            return true;
        }
        // SAFETY: cancel the write and wait for it to end, so nothing reads
        // the report once we return.
        unsafe {
            CancelIoEx(handle, &overlapped);
            GetOverlappedResult(handle, &overlapped, &mut put, 1);
        }
        false
    }

    /// Opens the pads not open yet, each with a reader thread.
    fn scan() {
        let paths = hid_paths();
        {
            let pads = lock();
            // A twin whose pad went away is opened again: over USB, say,
            // after the Bluetooth link dropped with the cable still in.
            TWINS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|(path, mac)| {
                    paths.contains(path) && pads.iter().any(|pad| pad.mac == Some(*mac))
                });
        }
        for path in paths {
            // Cheap filter before opening anything: Sony's vendor id is in
            // the path, as `vid_054c` (USB) or `vid&0002054c` (Bluetooth).
            let lower = String::from_utf16_lossy(&path).to_ascii_lowercase();
            if !lower.contains("054c") {
                continue;
            }
            if lock().iter().any(|pad| pad.path == path)
                || TWINS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .any(|(twin, _)| *twin == path)
            {
                continue;
            }
            let Some((handle, mut reports, pad, dongle)) = open(&path) else {
                continue;
            };
            // The Bluetooth HID service in the path; reports longer than USB's
            // 64 bytes are Bluetooth's too.
            let bluetooth = lower.contains("{00001124-") || reports.input.len() > 64;
            let link = Link {
                pad,
                dongle,
                bluetooth,
            };
            let name = pad.name();
            // A paired pad that is switched off can still be listed over
            // Bluetooth, and the DualShock 4 adapter without a pad: it does
            // not answer.
            if !full_reports(handle, pad, &mut reports.feature) {
                tracing::debug!(path = %lower, pad = name, "PlayStation pad listed but not answering");
                // SAFETY: just opened, nothing else has it.
                unsafe { CloseHandle(handle) };
                continue;
            }
            let mac = address(handle, pad, bluetooth, &mut reports.feature);
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = {
                let mut pads = lock();
                let twin = mac.is_some() && pads.iter().any(|pad| pad.mac == mac);
                (!twin).then(|| {
                    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                    pads.push(Pad {
                        id,
                        path: path.clone(),
                        mac,
                        state: None,
                        rumble: (0, 0),
                    });
                    id
                })
            };
            let Some(id) = id else {
                tracing::info!(path = %lower, pad = name, bluetooth, "PlayStation pad already open over the other transport");
                if let Some(mac) = mac {
                    TWINS
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((path, mac));
                }
                // SAFETY: just opened, nothing else has it.
                unsafe { CloseHandle(handle) };
                continue;
            };
            tracing::info!(path = %lower, pad = name, bluetooth, "PlayStation pad connected");
            let raw = handle as usize;
            if let Err(error) = std::thread::Builder::new()
                .name("PlayStation pad reader".into())
                .spawn(move || read(raw as HANDLE, id, link, reports))
            {
                lock().retain(|pad| pad.id != id);
                // SAFETY: just opened, and no reader has it.
                unsafe { CloseHandle(handle) };
                tracing::warn!(%error, "could not start a PlayStation pad reader");
            }
        }
    }

    /// What a reader needs to know about its pad.
    #[derive(Clone, Copy)]
    struct Link {
        pad: Sony,
        /// The DualShock 4 adapter, which reports while no pad is paired.
        dongle: bool,
        bluetooth: bool,
    }

    /// Reads reports until the pad goes away, then forgets it. Between
    /// reports it sends the pad the motor speeds the title asked for, when
    /// they changed: here rather than on the title's thread, which a write
    /// to a pad that went quiet would hold up.
    fn read(handle: HANDLE, id: u64, link: Link, mut reports: Reports) {
        let Link {
            pad: kind,
            dongle,
            bluetooth,
        } = link;
        // Sets the state, and tells the motor speeds the title wants.
        let set = |state: Option<PadState>| {
            lock().iter_mut().find(|pad| pad.id == id).map(|pad| {
                pad.state = state;
                pad.rumble
            })
        };
        // SAFETY: unnamed manual-reset events; no security attributes.
        let (event, wrote) = unsafe {
            (
                CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()),
                CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()),
            )
        };
        // The speeds the pad was sent last (a new connection's motors are
        // still) and the ones the title wants.
        let (mut sent, mut wanted) = ((0, 0), (0, 0));
        let mut write_failed = false;
        let why = 'pad: loop {
            if event.is_null() {
                break 'pad "no event";
            }
            if wanted != sent && reports.output > 0 && !wrote.is_null() {
                sent = wanted;
                let report = rumble_report(kind, bluetooth, reports.output, wanted.0, wanted.1);
                if !write(handle, wrote, &report) && !write_failed {
                    write_failed = true;
                    tracing::info!(
                        pad = kind.name(),
                        bluetooth,
                        "a PlayStation pad rumble report did not go out"
                    );
                }
            }
            // SAFETY: plain data; only the event is set.
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = event;
            let mut got = 0;
            // SAFETY: the buffer and `overlapped` outlive the read: every way
            // out below waits for it to end first.
            let started = unsafe {
                ReadFile(
                    handle,
                    reports.input.as_mut_ptr(),
                    reports.input.len() as u32,
                    std::ptr::null_mut(),
                    &mut overlapped,
                )
            };
            // SAFETY: plain query.
            if started == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
                break 'pad "read failed";
            }
            loop {
                // SAFETY: the read started above.
                let done =
                    unsafe { GetOverlappedResultEx(handle, &overlapped, &mut got, QUIET_MS, 0) };
                if done != 0 {
                    if got == 0 {
                        break 'pad "read nothing";
                    }
                    break;
                }
                // SAFETY: plain query.
                if unsafe { GetLastError() } != WAIT_TIMEOUT {
                    break 'pad "read failed";
                }
                // Silent: whatever was held is not held any more, and the
                // pad may be gone without the read failing.
                set(None);
                if !full_reports(handle, kind, &mut reports.feature) {
                    // SAFETY: cancel the read and wait for it to end, so
                    // nothing writes into the input buffer once we return.
                    unsafe {
                        CancelIoEx(handle, &overlapped);
                        GetOverlappedResult(handle, &overlapped, &mut got, 1);
                    }
                    break 'pad "not answering";
                }
            }
            let report = &reports.input[..got as usize];
            // The adapter without a pad: nothing held, and no pad listed.
            let state = if dongle && dongle_empty(report) {
                Some(None)
            } else {
                parse(kind, report, bluetooth).map(Some)
            };
            if let Some(state) = state
                && let Some(asked) = set(state)
            {
                wanted = asked;
            }
        };
        tracing::info!(
            why,
            pad = kind.name(),
            bluetooth,
            "PlayStation pad disconnected"
        );
        lock().retain(|pad| pad.id != id);
        // SAFETY: ours, and nothing uses them any more.
        unsafe {
            for event in [event, wrote] {
                if !event.is_null() {
                    CloseHandle(event);
                }
            }
            CloseHandle(handle);
        }
    }

    /// The pad at `path` opened for reading and writing, which pad it is and
    /// whether it is the DualShock 4 adapter; `None` when it is something
    /// else or cannot be opened.
    fn open(path: &[u16]) -> Option<(HANDLE, Reports, Sony, bool)> {
        let mut name = path.to_vec();
        name.push(0);
        // SAFETY: a terminated path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut attributes = HIDD_ATTRIBUTES {
            Size: size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        // SAFETY: an open device and room for its attributes.
        let ours = unsafe { HidD_GetAttributes(handle, &mut attributes) }
            .then(|| Sony::of(attributes.VendorID, attributes.ProductID))
            .flatten();
        if let Some(pad) = ours
            && let Some(reports) = Reports::for_device(handle)
        {
            let dongle = attributes.VendorID == SONY && attributes.ProductID == DONGLE;
            return Some((handle, reports, pad, dongle));
        }
        // SAFETY: just opened.
        unsafe { CloseHandle(handle) };
        None
    }

    /// The device paths of the HID devices present, without terminators.
    fn hid_paths() -> Vec<Vec<u16>> {
        let mut paths = Vec::new();
        // SAFETY: plain queries into buffers we own; the list is destroyed
        // before returning.
        unsafe {
            let mut guid = std::mem::zeroed();
            HidD_GetHidGuid(&mut guid);
            let set = SetupDiGetClassDevsW(
                &guid,
                std::ptr::null(),
                std::ptr::null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            );
            if set == INVALID_HANDLE_VALUE as isize {
                return paths;
            }
            for index in 0.. {
                let mut interface = SP_DEVICE_INTERFACE_DATA {
                    cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                    ..std::mem::zeroed()
                };
                if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut interface)
                    == 0
                {
                    break;
                }
                let mut size = 0;
                SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    std::ptr::null_mut(),
                    0,
                    &mut size,
                    std::ptr::null_mut(),
                );
                if size == 0 {
                    continue;
                }
                // u64s keep the detail's alignment.
                let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
                let detail = buffer
                    .as_mut_ptr()
                    .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                if SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    detail,
                    size,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ) == 0
                {
                    continue;
                }
                let start = std::ptr::addr_of!((*detail).DevicePath).cast::<u16>();
                let room = (size as usize
                    - std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath))
                    / 2;
                let wide = std::slice::from_raw_parts(start, room);
                let end = wide.iter().position(|&c| c == 0).unwrap_or(room);
                paths.push(wide[..end].to_vec());
            }
            SetupDiDestroyDeviceInfoList(set);
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_input_prefers_the_remapper_in_any_slot_and_falls_back_to_hid() {
        let physical = PadState {
            buttons: A,
            ..PadState::default()
        };
        let remapped = PadState {
            buttons: Y,
            ..PadState::default()
        };
        let mut assigned = [None; super::super::PADS];
        let none = [None; super::super::PADS];
        let xinput = [None, None, Some(remapped), None];

        assert_eq!(
            slots(Mode::Auto, none, &mut assigned, || vec![(1, physical)])[0],
            Some(physical)
        );
        assert_eq!(
            slots(Mode::Auto, xinput, &mut assigned, || panic!(
                "HID not needed"
            )),
            xinput
        );
        assert_eq!(
            slots(Mode::Off, none, &mut assigned, || panic!("HID disabled")),
            none
        );
        assert_eq!(
            slots(Mode::Auto, none, &mut assigned, || vec![(2, physical)])[0],
            Some(physical)
        );
    }

    #[test]
    fn a_surviving_hid_pad_keeps_its_slot_through_disconnect_and_reconnect() {
        let a = PadState {
            buttons: A,
            ..PadState::default()
        };
        let b = PadState {
            buttons: B,
            ..PadState::default()
        };
        let x = PadState {
            buttons: X,
            ..PadState::default()
        };
        let xinput = [None, Some(x), None, None];
        let mut assigned = [None; super::super::PADS];
        assert_eq!(
            slots(Mode::Both, xinput, &mut assigned, || vec![(7, a), (8, b)]),
            [Some(a), Some(x), Some(b), None]
        );
        assert_eq!(
            slots(Mode::Both, xinput, &mut assigned, || vec![(8, b)]),
            [None, Some(x), Some(b), None]
        );
        assert_eq!(
            slots(Mode::Both, xinput, &mut assigned, || vec![(8, b), (9, a)]),
            [Some(a), Some(x), Some(b), None]
        );
    }

    #[test]
    fn the_pairing_report_holds_the_address() {
        let mut pairing = [0u8; 20];
        pairing[0] = 0x09;
        pairing[1..7].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(mac(&pairing), Some([1, 2, 3, 4, 5, 6]));
        // a pad that answered with nothing cannot be told apart
        assert_eq!(mac(&[0x09; 1]), None);
        assert_eq!(mac(&[0x09, 0, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn a_usb_report_reads_as_a_pad() {
        let mut report = [0u8; 64];
        report[0] = 0x01;
        report[1..5].copy_from_slice(&[0, 255, 128, 128]); // left stick: left, down
        report[5] = 255; // L2
        report[8] = 0x20 | 2; // cross, D-pad right
        report[9] = 0x20 | 0x01; // options, L1
        let pad = parse(Sony::DualSense, &report, false).unwrap();
        assert_eq!(pad.buttons, A | RIGHT | MENU | LEFT_SHOULDER);
        assert_eq!(
            (pad.left_x, pad.left_y, pad.left_trigger),
            (-1.0, -1.0, 1.0)
        );
        assert!(pad.right_x.abs() < 0.01 && pad.right_y.abs() < 0.01);
    }

    #[test]
    fn bluetooth_reports_read_one_byte_further_in() {
        let mut report = [0u8; 78];
        report[0] = 0x31;
        report[2..6].copy_from_slice(&[128, 0, 255, 128]); // left up, right right
        report[7] = 255; // R2
        report[9] = 0x80 | 8; // triangle, D-pad centred
        let pad = parse(Sony::DualSense, &report, true).unwrap();
        assert_eq!(pad.buttons, Y);
        assert_eq!(
            (pad.left_y, pad.right_x, pad.right_trigger),
            (1.0, 1.0, 1.0)
        );
        // the same bytes over USB are not a pad report
        assert_eq!(parse(Sony::DualSense, &report, false), None);
    }

    #[test]
    fn the_short_bluetooth_report_reads_too() {
        let mut report = [0u8; 78];
        report[0] = 0x01;
        report[1..5].copy_from_slice(&[128; 4]);
        report[5] = 0x40 | 4; // circle, D-pad down
        report[7] = 0x02; // touchpad click
        report[8] = 128; // L2 halfway
        let pad = parse(Sony::DualSense, &report, true).unwrap();
        assert_eq!(pad.buttons, B | DOWN | VIEW);
        assert!((pad.left_trigger - 0.5).abs() < 0.01);
    }

    // Ported from VIscosity's hid.rs tests (commit d8db7a0), adapted to
    // this reader: sticks centre at 127.5 here, so 128 reads as nearly 0.

    /// A DualSense USB report: sticks, triggers, counter, three button
    /// bytes, the rest zero.
    fn dualsense_usb(sticks: [u8; 4], triggers: [u8; 2], buttons: [u8; 3]) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0] = 0x01;
        r[1..5].copy_from_slice(&sticks);
        r[5..7].copy_from_slice(&triggers);
        r[8..11].copy_from_slice(&buttons);
        r
    }

    fn at_rest(v: f32) -> bool {
        v.abs() < 0.01
    }

    #[test]
    fn a_dualsense_on_usb_reads_as_a_console_pad() {
        // Cross, R1, Options, D-pad down-left; left stick fully up and
        // left, right stick at rest, L2 full.
        let r = dualsense_usb([0, 0, 128, 128], [255, 0], [0x20 | 5, 0x02 | 0x20, 0]);
        let pad = parse(Sony::DualSense, &r, false).unwrap();
        assert_eq!(pad.buttons, A | RIGHT_SHOULDER | MENU | DOWN | LEFT);
        assert_eq!((pad.left_x, pad.left_y), (-1.0, 1.0));
        assert!(at_rest(pad.right_x) && at_rest(pad.right_y));
        assert_eq!((pad.left_trigger, pad.right_trigger), (1.0, 0.0));
    }

    #[test]
    fn a_dualsense_on_bluetooth_reads_in_both_report_forms() {
        // The full report: the USB layout one byte later.
        let usb = dualsense_usb([255, 255, 0, 255], [0, 128], [0x80 | 8, 0x40, 0x02]);
        let mut full = vec![0x31, 0x00];
        full.extend(&usb[1..]);
        full.resize(78, 0);
        let pad = parse(Sony::DualSense, &full, true).unwrap();
        assert_eq!(pad, parse(Sony::DualSense, &usb, false).unwrap());
        assert_eq!(pad.buttons, Y | LEFT_STICK | VIEW);
        assert_eq!(
            (pad.left_x, pad.left_y, pad.right_x, pad.right_y),
            (1.0, -1.0, -1.0, -1.0)
        );
        // The short report before the full ones are asked for: buttons
        // before triggers.
        let short = [0x01, 128, 128, 128, 128, 0x10, 0x01, 0, 0, 255];
        let pad = parse(Sony::DualSense, &short, true).unwrap();
        assert_eq!(pad.buttons, X | UP | LEFT_SHOULDER);
        assert_eq!(pad.right_trigger, 1.0);
    }

    #[test]
    fn a_dualshock_4_reads_on_usb_and_bluetooth() {
        let mut usb = vec![0u8; 64];
        usb[0] = 0x01;
        usb[1..5].copy_from_slice(&[128, 128, 255, 0]);
        usb[5] = 0x40 | 2; // circle, D-pad right
        usb[6] = 0x10 | 0x80; // share, R3
        usb[7] = 0x02; // touchpad click
        usb[8] = 0;
        usb[9] = 255;
        let pad = parse(Sony::DualShock4, &usb, false).unwrap();
        assert_eq!(pad.buttons, B | RIGHT | VIEW | RIGHT_STICK);
        assert_eq!(
            (pad.right_x, pad.right_y, pad.right_trigger),
            (1.0, 1.0, 1.0)
        );
        assert!(at_rest(pad.left_x) && at_rest(pad.left_y) && pad.left_trigger == 0.0);
        // The same 0x01 report on Bluetooth, before the full ones: the 10
        // bytes the pad sends then.
        assert_eq!(parse(Sony::DualShock4, &usb[..10], true), Some(pad));
        // The full Bluetooth report: two bytes further in.
        let mut bt = vec![0x11, 0xC0, 0x00];
        bt.extend(&usb[1..]);
        bt.resize(547, 0);
        assert_eq!(parse(Sony::DualShock4, &bt, true), Some(pad));
        // Reports that are not input state, or too short.
        assert_eq!(parse(Sony::DualShock4, &[0x05, 0, 0], false), None);
        assert_eq!(parse(Sony::DualShock4, &usb[..9], false), None);
        assert_eq!(parse(Sony::DualSense, &[], false), None);
        // A DualShock 4 report is not a DualSense's, nor the other way.
        assert_eq!(parse(Sony::DualSense, &bt, true), None);
        assert_eq!(parse(Sony::DualShock4, &[0x31; 78], true), None);
    }

    #[test]
    fn the_adapter_without_a_pad_reads_as_no_pad() {
        let mut report = [0u8; 64];
        report[0] = 0x01;
        assert!(!dongle_empty(&report));
        report[31] = 0x04;
        assert!(dongle_empty(&report));
        // only the 64-byte USB report says so
        assert!(!dongle_empty(&report[..32]));
    }

    #[test]
    fn a_bluetooth_serial_number_holds_the_address_last_byte_first() {
        let pairing = Some([0x56, 0x34, 0x12, 0xEF, 0xCD, 0xAB]);
        assert_eq!(serial_mac("abcdef123456"), pairing);
        assert_eq!(serial_mac("AB:CD:EF:12:34:56"), pairing);
        assert_eq!(serial_mac("ab-cd-ef-12-34-56"), pairing);
        assert_eq!(serial_mac("000000000000"), None);
        assert_eq!(serial_mac("abcdef12345"), None);
        assert_eq!(serial_mac("abcdef1234567"), None);
        assert_eq!(serial_mac("Wireless Controller"), None);
        assert_eq!(serial_mac(""), None);
    }

    #[test]
    fn the_crc_is_ieee_crc_32() {
        assert_eq!(crc32(&[b"1234", b"56789"]), 0xCBF4_3926);
    }

    #[test]
    fn rumble_reports_put_the_motors_where_sony_reads_them() {
        let r = rumble_report(Sony::DualSense, false, 48, 0xFF00, 0x8000);
        assert_eq!(r.len(), 48);
        assert_eq!(&r[..5], &[0x02, 0x03, 0x00, 0x80, 0xFF]);
        let r = rumble_report(Sony::DualShock4, false, 32, 0x1000, 0x2000);
        assert_eq!(r.len(), 32);
        assert_eq!(&r[..6], &[0x05, 0x01, 0x00, 0x00, 0x20, 0x10]);
        // Over Bluetooth the CRC ends the 78-byte report however long the
        // write is.
        for pad in [Sony::DualSense, Sony::DualShock4] {
            for length in [78, 547] {
                let r = rumble_report(pad, true, length, 0xFFFF, 0);
                assert_eq!(r.len(), length);
                let crc = crc32(&[&[0xA2], &r[..74]]);
                assert_eq!(&r[74..78], &crc.to_le_bytes());
                assert!(r[78..].iter().all(|&b| b == 0));
            }
        }
        let r = rumble_report(Sony::DualSense, true, 547, 0xFFFF, 0);
        assert_eq!(&r[..7], &[0x31, 0x00, 0x10, 0x03, 0x00, 0x00, 0xFF]);
        let r = rumble_report(Sony::DualShock4, true, 547, 0xFFFF, 0);
        assert_eq!(&r[..8], &[0x11, 0xC0, 0x00, 0x01, 0x00, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn sony_pads_are_known_by_vendor_and_product() {
        assert_eq!(Sony::of(0x054C, 0x0CE6), Some(Sony::DualSense));
        assert_eq!(Sony::of(0x054C, 0x0DF2), Some(Sony::DualSense));
        assert_eq!(Sony::of(0x054C, 0x05C4), Some(Sony::DualShock4));
        assert_eq!(Sony::of(0x054C, 0x09CC), Some(Sony::DualShock4));
        assert_eq!(Sony::of(0x054C, 0x0BA0), Some(Sony::DualShock4));
        assert_eq!(Sony::of(0x054C, 0x0268), None); // DualShock 3
        assert_eq!(Sony::of(0x045E, 0x0B12), None);
    }

    /// With a pad connected: rumbles each PlayStation pad for a second, then
    /// prints every change of state for `VICTORY_PAD_SECONDS` (default 20).
    #[cfg(windows)]
    #[test]
    #[ignore = "needs a pad: cargo test -p victory-hle connected_playstation_pads -- --ignored --nocapture"]
    fn connected_playstation_pads() {
        use std::time::{Duration, Instant};
        let found = Instant::now();
        while pads().is_empty() && found.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let ids: Vec<u64> = pads().iter().map(|&(id, _)| id).collect();
        eprintln!("PlayStation pads: {ids:?}");
        for &id in &ids {
            rumble(id, 0xFFFF, 0x8000);
        }
        std::thread::sleep(Duration::from_secs(1));
        for &id in &ids {
            rumble(id, 0, 0);
        }
        let seconds = std::env::var("VICTORY_PAD_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        let end = Instant::now() + Duration::from_secs(seconds);
        let mut last = Vec::new();
        while Instant::now() < end {
            let now = pads();
            if now != last {
                for (id, pad) in &now {
                    eprintln!(
                        "pad {id}: buttons {:#06x} lt {:.2} rt {:.2} left ({:.2}, {:.2}) right ({:.2}, {:.2})",
                        pad.buttons,
                        pad.left_trigger,
                        pad.right_trigger,
                        pad.left_x,
                        pad.left_y,
                        pad.right_x,
                        pad.right_y
                    );
                }
                last = now;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    }
}
