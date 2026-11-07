// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{Seek, SeekFrom, Write};

use tempfile::{Builder, NamedTempFile};

pub struct DngSpec {
    pub width: u32,
    pub height: u32,
    pub bits: u16,
    pub black: u16,
    pub white: u32,
    pub cfa: [u8; 4],
    pub crop: Option<[u32; 4]>,
    /// DNG ActiveArea in top, left, bottom, right order.
    pub active_area: Option<[u32; 4]>,
    /// Header geometry used for overflow tests when it differs from the tiny payload.
    pub header_dimensions: Option<[u32; 2]>,
    /// Omit ImageWidth to exercise named metadata refusal.
    pub omit_width: bool,
    pub capture_time: Option<String>,
    pub modify_time: Option<String>,
    pub serial_number: Option<String>,
    pub make: String,
    pub model: String,
    pub samples: Vec<u16>,
    /// DNG AsShotNeutral; the as-shot white balance is its reciprocal.
    pub as_shot_neutral: Option<[f32; 3]>,
    /// Row-major ColorMatrix1 (XYZ to camera, D65); the identity when absent.
    pub color_matrix: Option<[f32; 9]>,
    /// Row-major ColorMatrix2 and its CalibrationIlluminant2 code.
    pub color_matrix2: Option<([f32; 9], u16)>,
    /// Row-major CameraCalibration1 and CameraCalibration2, beside the
    /// ColorMatrix of the same slot.
    pub camera_calibration: [Option<[f32; 9]>; 2],
    /// DNG AnalogBalance.
    pub analog_balance: Option<[f32; 3]>,
    /// Write no ColorMatrix1 and no CalibrationIlluminant1.
    pub omit_color_matrix: bool,
    /// PhotometricInterpretation 1 (BlackIsZero); rawler's DNG decoder still
    /// wants the CFA tags.
    pub monochrome: bool,
    /// EXIF Orientation of the frame.
    pub orientation: Option<u16>,
}

impl DngSpec {
    pub fn new(bits: u16, black: u16, white: u32, cfa: [u8; 4], samples: Vec<u16>) -> Self {
        Self {
            width: 8,
            height: 8,
            bits,
            black,
            white,
            cfa,
            crop: None,
            active_area: None,
            header_dimensions: None,
            omit_width: false,
            capture_time: None,
            modify_time: None,
            serial_number: None,
            make: "FocusForge Test".to_string(),
            model: "Synthetic Bayer".to_string(),
            samples,
            as_shot_neutral: None,
            color_matrix: None,
            color_matrix2: None,
            camera_calibration: [None, None],
            analog_balance: None,
            omit_color_matrix: false,
            monochrome: false,
            orientation: None,
        }
    }
}

struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    value: Vec<u8>,
}

pub fn write_dng(spec: &DngSpec, truncate_payload: bool) -> NamedTempFile {
    assert_eq!(spec.samples.len(), (spec.width * spec.height) as usize);
    assert!(matches!(spec.bits, 12 | 14));
    let payload = packed_samples(&spec.samples, spec.bits);
    let matrix = spec
        .color_matrix
        .unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0])
        .map(|value| ((value * 10_000.0).round() as i32, 10_000_i32));
    let [header_width, header_height] = spec.header_dimensions.unwrap_or([spec.width, spec.height]);
    let has_exif = spec.capture_time.is_some() || spec.serial_number.is_some();
    let mut entries = vec![
        Entry::long(257, &[spec.height]),
        Entry::short(258, &[spec.bits]),
        Entry::short(259, &[1]),
        Entry::short(262, &[if spec.monochrome { 1 } else { 32803 }]),
        Entry::ascii(271, &spec.make),
        Entry::ascii(272, &spec.model),
        Entry::long(273, &[0]),
        Entry::short(277, &[1]),
        Entry::long(278, &[spec.height]),
        Entry::long(279, &[payload.len() as u32]),
        Entry::short(284, &[1]),
        Entry::byte(50706, &[1, 4, 0, 0]),
        Entry::ascii(50708, &spec.model),
        Entry::byte(50710, &[0, 1, 2]),
        Entry::short(50711, &[1]),
        Entry::short(50713, &[2, 2]),
        Entry::short(50714, &[spec.black; 4]),
        Entry::long(50717, &[spec.white]),
    ];
    if !spec.omit_color_matrix {
        entries.push(Entry::srational(50721, &matrix));
        entries.push(Entry::short(50778, &[21]));
    }
    if let Some((matrix2, illuminant)) = spec.color_matrix2 {
        entries.push(Entry::srational(
            50722,
            &matrix2.map(|value| ((value * 10_000.0).round() as i32, 10_000_i32)),
        ));
        entries.push(Entry::short(50779, &[illuminant]));
    }
    for (slot, calibration) in spec.camera_calibration.iter().enumerate() {
        if let Some(calibration) = calibration {
            entries.push(Entry::srational(
                50723 + slot as u16,
                &calibration.map(|value| ((value * 10_000.0).round() as i32, 10_000_i32)),
            ));
        }
    }
    if let Some(balance) = spec.analog_balance {
        entries.push(Entry::rational(
            50727,
            &balance.map(|value| ((value * 10_000.0).round() as u32, 10_000_u32)),
        ));
    }
    entries.push(Entry::short(33421, &[2, 2]));
    entries.push(Entry::byte(33422, &spec.cfa));
    if let Some(orientation) = spec.orientation {
        entries.push(Entry::short(274, &[orientation]));
    }
    if let Some(neutral) = spec.as_shot_neutral {
        entries.push(Entry::rational(
            50728,
            &neutral.map(|value| ((value * 10_000.0).round() as u32, 10_000_u32)),
        ));
    }
    if let Some(modify_time) = &spec.modify_time {
        entries.push(Entry::ascii(306, modify_time));
    }
    if has_exif {
        entries.push(Entry::long(34665, &[0]));
    }
    if !spec.omit_width {
        entries.push(Entry::long(256, &[header_width]));
    }
    if spec.header_dimensions.is_some() {
        let height = entries
            .iter_mut()
            .find(|entry| entry.tag == 257)
            .expect("image length entry");
        height.value = header_height.to_le_bytes().to_vec();
    }
    if let Some([x, y, width, height]) = spec.crop {
        entries.push(Entry::long(50719, &[x, y]));
        entries.push(Entry::long(50720, &[width, height]));
    }
    if let Some(active_area) = spec.active_area {
        entries.push(Entry::long(50829, &active_area));
    }
    entries.sort_by_key(|entry| entry.tag);

    let ifd_start = 8_usize;
    let entry_start = ifd_start + 2;
    let after_ifd = entry_start + entries.len() * 12 + 4;
    let mut bytes = vec![0; after_ifd];
    bytes[..2].copy_from_slice(b"II");
    bytes[2..4].copy_from_slice(&42_u16.to_le_bytes());
    bytes[4..8].copy_from_slice(&(ifd_start as u32).to_le_bytes());
    bytes[ifd_start..ifd_start + 2].copy_from_slice(&(entries.len() as u16).to_le_bytes());

    let mut extra = after_ifd;
    let mut strip_offset_field = None;
    let mut exif_offset_field = None;
    for (index, entry) in entries.iter().enumerate() {
        let at = entry_start + index * 12;
        bytes[at..at + 2].copy_from_slice(&entry.tag.to_le_bytes());
        bytes[at + 2..at + 4].copy_from_slice(&entry.kind.to_le_bytes());
        bytes[at + 4..at + 8].copy_from_slice(&entry.count.to_le_bytes());
        if entry.value.len() <= 4 {
            bytes[at + 8..at + 8 + entry.value.len()].copy_from_slice(&entry.value);
        } else {
            if !extra.is_multiple_of(2) {
                bytes.push(0);
                extra += 1;
            }
            bytes[at + 8..at + 12].copy_from_slice(&(extra as u32).to_le_bytes());
            bytes.extend_from_slice(&entry.value);
            extra += entry.value.len();
        }
        if entry.tag == 273 {
            strip_offset_field = Some(at + 8);
        }
        if entry.tag == 34665 {
            exif_offset_field = Some(at + 8);
        }
    }
    if has_exif {
        let exif_start = bytes.len();
        bytes[exif_offset_field.expect("EXIF offset field")..][..4]
            .copy_from_slice(&(exif_start as u32).to_le_bytes());
        let mut exif_entries = Vec::new();
        if let Some(capture_time) = &spec.capture_time {
            exif_entries.push(Entry::ascii(36867, capture_time));
        }
        if let Some(serial_number) = &spec.serial_number {
            exif_entries.push(Entry::ascii(42033, serial_number));
        }
        exif_entries.sort_by_key(|entry| entry.tag);
        bytes.extend_from_slice(&(exif_entries.len() as u16).to_le_bytes());
        let exif_entry_start = bytes.len();
        bytes.resize(bytes.len() + exif_entries.len() * 12 + 4, 0);
        let mut exif_extra = bytes.len();
        for (index, entry) in exif_entries.iter().enumerate() {
            let at = exif_entry_start + index * 12;
            bytes[at..at + 2].copy_from_slice(&entry.tag.to_le_bytes());
            bytes[at + 2..at + 4].copy_from_slice(&entry.kind.to_le_bytes());
            bytes[at + 4..at + 8].copy_from_slice(&entry.count.to_le_bytes());
            if entry.value.len() <= 4 {
                bytes[at + 8..at + 8 + entry.value.len()].copy_from_slice(&entry.value);
            } else {
                bytes[at + 8..at + 12].copy_from_slice(&(exif_extra as u32).to_le_bytes());
                bytes.extend_from_slice(&entry.value);
                exif_extra += entry.value.len();
            }
        }
    }
    let payload_offset = bytes.len() as u32;
    bytes[strip_offset_field.expect("strip offset field")..][..4]
        .copy_from_slice(&payload_offset.to_le_bytes());
    if !truncate_payload {
        bytes.extend_from_slice(&payload);
    }

    let mut builder = Builder::new();
    builder.suffix(".dng");
    // Only integration tests get a target tmp directory; unit tests fall back to the system one.
    let mut file = match option_env!("CARGO_TARGET_TMPDIR") {
        Some(dir) => builder.tempfile_in(dir),
        None => builder.tempfile(),
    }
    .expect("temporary DNG");
    file.write_all(&bytes).expect("write DNG");
    file.flush().expect("flush DNG");
    file.as_file_mut()
        .seek(SeekFrom::Start(0))
        .expect("rewind DNG");
    file
}

impl Entry {
    fn byte(tag: u16, values: &[u8]) -> Self {
        Self {
            tag,
            kind: 1,
            count: values.len() as u32,
            value: values.to_vec(),
        }
    }

    fn ascii(tag: u16, value: &str) -> Self {
        let mut value = value.as_bytes().to_vec();
        value.push(0);
        Self {
            tag,
            kind: 2,
            count: value.len() as u32,
            value,
        }
    }

    fn short(tag: u16, values: &[u16]) -> Self {
        Self {
            tag,
            kind: 3,
            count: values.len() as u32,
            value: values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        }
    }

    fn long(tag: u16, values: &[u32]) -> Self {
        Self {
            tag,
            kind: 4,
            count: values.len() as u32,
            value: values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        }
    }

    fn rational(tag: u16, values: &[(u32, u32)]) -> Self {
        Self {
            tag,
            kind: 5,
            count: values.len() as u32,
            value: values
                .iter()
                .flat_map(|(numerator, denominator)| {
                    numerator
                        .to_le_bytes()
                        .into_iter()
                        .chain(denominator.to_le_bytes())
                })
                .collect(),
        }
    }

    fn srational(tag: u16, values: &[(i32, i32)]) -> Self {
        Self {
            tag,
            kind: 10,
            count: values.len() as u32,
            value: values
                .iter()
                .flat_map(|(numerator, denominator)| {
                    numerator
                        .to_le_bytes()
                        .into_iter()
                        .chain(denominator.to_le_bytes())
                })
                .collect(),
        }
    }
}

fn packed_samples(samples: &[u16], bits: u16) -> Vec<u8> {
    let mut output = Vec::with_capacity(samples.len() * bits as usize / 8);
    let mut pending = 0_u32;
    let mut pending_bits = 0_u16;
    for &sample in samples {
        assert!(sample < (1_u16 << bits));
        pending = (pending << bits) | u32::from(sample);
        pending_bits += bits;
        while pending_bits >= 8 {
            pending_bits -= 8;
            output.push((pending >> pending_bits) as u8);
            pending &= (1_u32 << pending_bits) - 1;
        }
    }
    if pending_bits != 0 {
        output.push((pending << (8 - pending_bits)) as u8);
    }
    output
}
