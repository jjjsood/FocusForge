// SPDX-License-Identifier: GPL-3.0-or-later

//! A source frame's descriptive metadata: EXIF, GPS, an ICC profile, an XMP
//! packet and an IPTC block, read once from a frame's header and carried
//! into an export. Never holds pixel data.

use std::path::Path;

use chrono::{DateTime, FixedOffset};
use rawler::exif::{Exif, ExifGPS};
use rawler::formats::tiff::{Rational, SRational};
use rawler::tags::{ExifGpsTag, ExifTag};

use super::paths::{RAW_EXTENSIONS, has_extension};
use super::raw::RawStack;
use super::tiff::header::{self, TAG_ICC_PROFILE, TAG_IPTC, TAG_XMP, TiffHeader};
use super::tiff::writer::ifd0_with_sub_ifds;

/// One IFD entry, exactly as a TIFF or EXIF sub-IFD declares it: a tag, its
/// TIFF field type and its value's element count. `value` is little-endian
/// in every case, even when the source file was big-endian (`MM`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagEntry {
    pub tag: u16,
    pub field_type: u16,
    pub count: u32,
    pub value: Vec<u8>,
}

/// Which reader produced a [`SourceMetadata`]. A RAW source has no XMP and
/// no IPTC (rawler surfaces no XMP packet through the fields this project
/// reads), and its EXIF comes from rawler's typed `Exif` rather than the
/// hand-written IFD walker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceKind {
    #[default]
    Tiff,
    Raw,
}

/// One source frame's descriptive metadata for export: IFD0's whitelisted
/// entries, the EXIF and GPS sub-IFDs, and the ICC/XMP/IPTC blocks as
/// opaque bytes. A block that fails to read (a junk pointer, a value past
/// EOF) is missing here and named in `warnings`, never a reason to fail the
/// whole read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceMetadata {
    pub ifd0: Vec<TagEntry>,
    pub exif: Vec<TagEntry>,
    pub gps: Vec<TagEntry>,
    pub icc: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
    pub iptc: Option<Vec<u8>>,
    pub kind: SourceKind,
    pub warnings: Vec<String>,
}

/// Extra TIFF entries an export adds beside a `StripWriter`'s fixed
/// 13-tag layout: IFD0 additions, plus the EXIF and GPS sub-IFDs a
/// non-empty list here gets a 34665/34853 pointer for. Empty by default --
/// an export with nothing to carry costs the writer nothing beyond this
/// field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataTags {
    pub ifd0: Vec<TagEntry>,
    pub exif: Vec<TagEntry>,
    pub gps: Vec<TagEntry>,
}

// --- export rewrite --------------------------------------------------------
//
// A source's `Software`, `DateTime` and `OffsetTime` describe *its* export,
// not this one, and its EXIF image-dimension tags may name a size a crop has
// since changed, so those are rewritten. Every other entry --
// `DateTimeOriginal` above all -- passes through untouched.

/// The export moment as EXIF `DateTime`/`DateTimeOriginal` text
/// (`YYYY:MM:DD HH:MM:SS`), in `now`'s own zone.
fn exif_datetime(now: DateTime<FixedOffset>) -> String {
    now.format("%Y:%m:%d %H:%M:%S").to_string()
}

fn set_entry(entries: &mut Vec<TagEntry>, entry: TagEntry) {
    if let Some(existing) = entries.iter_mut().find(|e| e.tag == entry.tag) {
        *existing = entry;
    } else {
        entries.push(entry);
    }
}

/// Rewrites `source` for one export: `Software` (305) and `DateTime` (306)
/// become the export's own identity and moment, `OffsetTime` (36880)
/// becomes that moment's UTC offset, and the EXIF image-dimension tags
/// (40962/40963) become `width`/`height`. `now` is read once by the caller,
/// not here: `chrono`'s local-time lookup is not safe to call concurrently
/// with anything that mutates `TZ`.
pub(crate) fn rewrite(
    source: &SourceMetadata,
    width: u32,
    height: u32,
    now: DateTime<FixedOffset>,
) -> MetadataTags {
    let mut ifd0 = source.ifd0.clone();
    set_entry(
        &mut ifd0,
        exif_tag_entry(
            ExifTag::Software,
            &format!("FocusForge {}", env!("CARGO_PKG_VERSION")),
        ),
    );
    set_entry(
        &mut ifd0,
        exif_tag_entry(ExifTag::ModifyDate, &exif_datetime(now)),
    );

    let mut exif = source.exif.clone();
    set_entry(
        &mut exif,
        exif_tag_entry(ExifTag::OffsetTime, &now.format("%:z").to_string()),
    );
    set_entry(&mut exif, exif_tag_entry(ExifTag::ExifImageWidth, &width));
    set_entry(&mut exif, exif_tag_entry(ExifTag::ExifImageHeight, &height));

    MetadataTags {
        ifd0,
        exif,
        gps: source.gps.clone(),
    }
}

/// The EXIF sub-IFD's bytes for embedding into a JPEG APP1 `Exif` segment: a
/// standalone classic-TIFF blob (`II`, IFD0 at offset 8, its EXIF and GPS
/// sub-IFDs immediately after) built by the same [`ifd0_with_sub_ifds`] a
/// TIFF export's own IFD0 goes through.
pub(crate) fn exif_blob(tags: &MetadataTags) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend(ifd0_with_sub_ifds(&tags.ifd0, &tags.exif, &tags.gps, 8, 0));
    out
}

// --- RAW-source mapping ---------------------------------------------------
//
// rawler's typed `Exif`/`ExifGPS` is the one EXIF route every RAW format
// shares: only the DNG decoder exposes a raw EXIF IFD, so a hand-written IFD
// walk like the TIFF reader's would miss every other RAW format.
// `ExifValue` gives each Rust type in `Exif`/`ExifGPS` one shared TIFF
// encoding.

const FIELD_TYPE_BYTE: u16 = 1;
const FIELD_TYPE_ASCII: u16 = 2;
const FIELD_TYPE_SHORT: u16 = 3;
const FIELD_TYPE_LONG: u16 = 4;
const FIELD_TYPE_RATIONAL: u16 = 5;
const FIELD_TYPE_UNDEFINED: u16 = 7;
const FIELD_TYPE_SSHORT: u16 = 8;
const FIELD_TYPE_SRATIONAL: u16 = 10;

/// One EXIF/GPS field's TIFF field type, element count and little-endian
/// bytes, shared by every type `rawler::exif::Exif`/`ExifGPS` uses.
trait ExifValue {
    fn field_type(&self) -> u16;
    fn count(&self) -> u32;
    fn bytes(&self) -> Vec<u8>;
}

impl ExifValue for u8 {
    fn field_type(&self) -> u16 {
        FIELD_TYPE_BYTE
    }
    fn count(&self) -> u32 {
        1
    }
    fn bytes(&self) -> Vec<u8> {
        vec![*self]
    }
}

impl ExifValue for u16 {
    fn field_type(&self) -> u16 {
        FIELD_TYPE_SHORT
    }
    fn count(&self) -> u32 {
        1
    }
    fn bytes(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl ExifValue for u32 {
    fn field_type(&self) -> u16 {
        FIELD_TYPE_LONG
    }
    fn count(&self) -> u32 {
        1
    }
    fn bytes(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl ExifValue for String {
    fn field_type(&self) -> u16 {
        FIELD_TYPE_ASCII
    }
    fn count(&self) -> u32 {
        self.len() as u32 + 1
    }
    fn bytes(&self) -> Vec<u8> {
        let mut bytes = self.as_bytes().to_vec();
        bytes.push(0);
        bytes
    }
}

impl ExifValue for Rational {
    fn field_type(&self) -> u16 {
        FIELD_TYPE_RATIONAL
    }
    fn count(&self) -> u32 {
        1
    }
    fn bytes(&self) -> Vec<u8> {
        [self.n.to_le_bytes(), self.d.to_le_bytes()].concat()
    }
}

impl ExifValue for SRational {
    fn field_type(&self) -> u16 {
        FIELD_TYPE_SRATIONAL
    }
    fn count(&self) -> u32 {
        1
    }
    fn bytes(&self) -> Vec<u8> {
        [self.n.to_le_bytes(), self.d.to_le_bytes()].concat()
    }
}

impl<const N: usize> ExifValue for [Rational; N] {
    fn field_type(&self) -> u16 {
        FIELD_TYPE_RATIONAL
    }
    fn count(&self) -> u32 {
        N as u32
    }
    fn bytes(&self) -> Vec<u8> {
        self.iter().flat_map(ExifValue::bytes).collect()
    }
}

impl ExifValue for [u8; 4] {
    fn field_type(&self) -> u16 {
        FIELD_TYPE_BYTE
    }
    fn count(&self) -> u32 {
        4
    }
    fn bytes(&self) -> Vec<u8> {
        self.to_vec()
    }
}

impl ExifValue for Vec<i16> {
    fn field_type(&self) -> u16 {
        FIELD_TYPE_SSHORT
    }
    fn count(&self) -> u32 {
        self.len() as u32
    }
    fn bytes(&self) -> Vec<u8> {
        self.iter().flat_map(|value| value.to_le_bytes()).collect()
    }
}

impl ExifValue for Vec<u8> {
    fn field_type(&self) -> u16 {
        FIELD_TYPE_UNDEFINED
    }
    fn count(&self) -> u32 {
        self.len() as u32
    }
    fn bytes(&self) -> Vec<u8> {
        self.clone()
    }
}

fn exif_tag_entry<T: ExifValue>(tag: ExifTag, value: &T) -> TagEntry {
    TagEntry {
        tag: tag as u16,
        field_type: value.field_type(),
        count: value.count(),
        value: value.bytes(),
    }
}

fn gps_tag_entry<T: ExifValue>(tag: ExifGpsTag, value: &T) -> TagEntry {
    TagEntry {
        tag: tag as u16,
        field_type: value.field_type(),
        count: value.count(),
        value: value.bytes(),
    }
}

/// Push one `TagEntry` per `Some` field the table lists, in table order.
macro_rules! push_exif_fields {
    ($entries:expr, $exif:expr, { $($field:ident => $tag:expr),* $(,)? }) => {
        $(
            if let Some(value) = &$exif.$field {
                $entries.push(exif_tag_entry($tag, value));
            }
        )*
    };
}

/// Push one `TagEntry` per `Some` GPS field the table lists, in table order.
macro_rules! push_gps_fields {
    ($entries:expr, $gps:expr, { $($field:ident => $tag:expr),* $(,)? }) => {
        $(
            if let Some(value) = &$gps.$field {
                $entries.push(gps_tag_entry($tag, value));
            }
        )*
    };
}

/// The CC0-1.0 "micro" sRGB v2 profile from saucecontrol/Compact-ICC-Profiles
/// (<https://github.com/saucecontrol/Compact-ICC-Profiles>), embedded as the
/// fallback for a RAW source: rawler develops every RAW format to sRGB
/// primaries and sRGB gamma (`RawDevelop`'s `SRgb` step), but no RAW format
/// carries its own ICC profile to say so.
const BUILTIN_SRGB_ICC: &[u8] = include_bytes!("srgb-v2-micro.icc");

/// Maps rawler's typed EXIF onto this project's `SourceMetadata`: `make`
/// and `model` (not part of `Exif`) go into IFD0 alongside `Orientation`,
/// `Artist` and `Copyright`; every other populated `Exif`/`ExifGPS` field
/// goes into the EXIF or GPS list. A RAW source has no XMP or IPTC and gets
/// the built-in sRGB profile rather than its own.
pub(crate) fn exif_to_metadata(exif: &Exif, make: &str, model: &str) -> SourceMetadata {
    let mut ifd0 = Vec::new();
    if !make.is_empty() {
        ifd0.push(exif_tag_entry(ExifTag::Make, &make.to_string()));
    }
    if !model.is_empty() {
        ifd0.push(exif_tag_entry(ExifTag::Model, &model.to_string()));
    }
    push_exif_fields!(ifd0, exif, {
        orientation => ExifTag::Orientation,
        artist => ExifTag::Artist,
        copyright => ExifTag::Copyright,
    });

    let mut exif_entries = Vec::new();
    push_exif_fields!(exif_entries, exif, {
        lens_spec => ExifTag::LensSpecification,
        exposure_time => ExifTag::ExposureTime,
        fnumber => ExifTag::FNumber,
        aperture_value => ExifTag::ApertureValue,
        brightness_value => ExifTag::BrightnessValue,
        iso_speed_ratings => ExifTag::ISOSpeedRatings,
        iso_speed => ExifTag::ISOSpeed,
        recommended_exposure_index => ExifTag::RecommendedExposureIndex,
        sensitivity_type => ExifTag::SensitivityType,
        exposure_bias => ExifTag::ExposureBiasValue,
        date_time_original => ExifTag::DateTimeOriginal,
        create_date => ExifTag::CreateDate,
        modify_date => ExifTag::ModifyDate,
        exposure_program => ExifTag::ExposureProgram,
        timezone_offset => ExifTag::TimeZoneOffset,
        offset_time => ExifTag::OffsetTime,
        offset_time_original => ExifTag::OffsetTimeOriginal,
        offset_time_digitized => ExifTag::OffsetTimeDigitized,
        sub_sec_time => ExifTag::SubSecTime,
        sub_sec_time_original => ExifTag::SubSecTimeOriginal,
        sub_sec_time_digitized => ExifTag::SubSecTimeDigitized,
        shutter_speed_value => ExifTag::ShutterSpeedValue,
        max_aperture_value => ExifTag::MaxApertureValue,
        subject_distance => ExifTag::SubjectDistance,
        metering_mode => ExifTag::MeteringMode,
        light_source => ExifTag::LightSource,
        flash => ExifTag::Flash,
        focal_length => ExifTag::FocalLength,
        image_number => ExifTag::ImageNumber,
        color_space => ExifTag::ColorSpace,
        flash_energy => ExifTag::FlashEnergy,
        exposure_mode => ExifTag::ExposureMode,
        white_balance => ExifTag::WhiteBalance,
        scene_capture_type => ExifTag::SceneCaptureType,
        subject_distance_range => ExifTag::SubjectDistanceRange,
        owner_name => ExifTag::OwnerName,
        serial_number => ExifTag::SerialNumber,
        lens_serial_number => ExifTag::LensSerialNumber,
        lens_make => ExifTag::LensMake,
        lens_model => ExifTag::LensModel,
        user_comment => ExifTag::UserComment,
    });

    let mut gps_entries = Vec::new();
    if let Some(gps) = &exif.gps {
        push_gps_fields_table(&mut gps_entries, gps);
    }

    SourceMetadata {
        ifd0,
        exif: exif_entries,
        gps: gps_entries,
        icc: Some(BUILTIN_SRGB_ICC.to_vec()),
        xmp: None,
        iptc: None,
        kind: SourceKind::Raw,
        warnings: Vec::new(),
    }
}

fn push_gps_fields_table(entries: &mut Vec<TagEntry>, gps: &ExifGPS) {
    push_gps_fields!(entries, gps, {
        gps_version_id => ExifGpsTag::GPSVersionID,
        gps_latitude_ref => ExifGpsTag::GPSLatitudeRef,
        gps_latitude => ExifGpsTag::GPSLatitude,
        gps_longitude_ref => ExifGpsTag::GPSLongitudeRef,
        gps_longitude => ExifGpsTag::GPSLongitude,
        gps_altitude_ref => ExifGpsTag::GPSAltitudeRef,
        gps_altitude => ExifGpsTag::GPSAltitude,
        gps_timestamp => ExifGpsTag::GPSTimeStamp,
        gps_satellites => ExifGpsTag::GPSSatellites,
        gps_status => ExifGpsTag::GPSStatus,
        gps_measure_mode => ExifGpsTag::GPSMeasureMode,
        gps_dop => ExifGpsTag::GPSDOP,
        gps_speed_ref => ExifGpsTag::GPSSpeedRef,
        gps_speed => ExifGpsTag::GPSSpeed,
        gps_track_ref => ExifGpsTag::GPSTrackRef,
        gps_track => ExifGpsTag::GPSTrack,
        gps_img_direction_ref => ExifGpsTag::GPSImgDirectionRef,
        gps_img_direction => ExifGpsTag::GPSImgDirection,
        gps_map_datum => ExifGpsTag::GPSMapDatum,
        gps_dest_latitude_ref => ExifGpsTag::GPSDestLatitudeRef,
        gps_dest_latitude => ExifGpsTag::GPSDestLatitude,
        gps_dest_longitude_ref => ExifGpsTag::GPSDestLongitudeRef,
        gps_dest_longitude => ExifGpsTag::GPSDestLongitude,
        gps_dest_bearing_ref => ExifGpsTag::GPSDestBearingRef,
        gps_dest_bearing => ExifGpsTag::GPSDestBearing,
        gps_dest_distance_ref => ExifGpsTag::GPSDestDistanceRef,
        gps_dest_distance => ExifGpsTag::GPSDestDistance,
        gps_processing_method => ExifGpsTag::GPSProcessingMethod,
        gps_area_information => ExifGpsTag::GPSAreaInformation,
        gps_date_stamp => ExifGpsTag::GPSDateStamp,
        gps_differential => ExifGpsTag::GPSDifferential,
        gps_h_positioning_error => ExifGpsTag::GPSHPositioningError,
    });
}

/// Maps a RAW frame's decoder metadata onto `SourceMetadata`.
pub(crate) fn from_raw(raw: &super::raw::RawMetadata) -> SourceMetadata {
    exif_to_metadata(&raw.exif, &raw.make, &raw.model)
}

/// Reads one export's source frame: TIFF through the TIFF header's own
/// metadata reader, RAW through the RAW decoder's metadata mapping, chosen
/// by `path`'s extension. Never fails: a source that cannot be opened at all
/// yields an empty [`SourceMetadata`] with one warning, exactly like a
/// damaged block within an otherwise readable file.
pub fn read_source_metadata(path: &Path) -> SourceMetadata {
    if has_extension(path, RAW_EXTENSIONS) {
        return match RawStack::open_files(&[path.to_path_buf()]) {
            Ok(stack) => match stack.metadata(0) {
                Some(raw) => from_raw(raw),
                None => SourceMetadata {
                    warnings: vec![format!("{}: no RAW metadata decoded", path.display())],
                    ..Default::default()
                },
            },
            Err(err) => SourceMetadata {
                warnings: vec![format!("{}: {err}", path.display())],
                ..Default::default()
            },
        };
    }
    let header = match TiffHeader::open(path) {
        Ok(header) => header,
        Err(err) => {
            return SourceMetadata {
                warnings: vec![format!("{}: {err}", path.display())],
                ..Default::default()
            };
        }
    };
    match std::fs::File::open(path) {
        Ok(file) => header::read_tiff_metadata(&file, header.byte_order, header.ifd_offset, path),
        Err(err) => SourceMetadata {
            warnings: vec![format!("{}: {err}", path.display())],
            ..Default::default()
        },
    }
}

// --- export plan ------------------------------------------------------------

/// Where an export's ICC profile came from, or why it has none. `Off`
/// mirrors the ICC switch itself, so a caller never has to consult it
/// alongside a separate on/off flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum IccOrigin {
    #[default]
    Off,
    Source,
    BuiltinSrgb,
    None,
}

/// One export's metadata, resolved from its source but not yet written:
/// format-neutral, so a TIFF/PNG/JPEG writer each apply their own container
/// limits (a PNG `iTXt` chunk needing valid UTF-8, an APP segment's byte
/// cap, PNG having no IPTC chunk at all) and decide the final per-block
/// outcome themselves.
#[derive(Debug, Clone, Default)]
pub(crate) struct MetadataPlan {
    /// Rewritten IFD0/EXIF/GPS entries, empty when the EXIF switch is off
    /// or the source had nothing to carry.
    pub tags: MetadataTags,
    pub has_exif: bool,
    pub icc: IccOrigin,
    pub icc_bytes: Option<Vec<u8>>,
    pub has_xmp: bool,
    pub xmp_bytes: Option<Vec<u8>>,
    pub has_iptc: bool,
    pub iptc_bytes: Option<Vec<u8>>,
}

/// Resolves `source` against the four metadata switches into a
/// [`MetadataPlan`]: `rewrite` only runs when the EXIF switch is on *and*
/// the source has something to carry, so a metadata-free source stays
/// exactly as untouched as an EXIF-switched-off one.
#[allow(clippy::too_many_arguments)] // one flag apiece; the four switches are the whole point
pub(crate) fn resolve_plan(
    source: &SourceMetadata,
    width: u32,
    height: u32,
    now: DateTime<FixedOffset>,
    exif_on: bool,
    icc_on: bool,
    xmp_on: bool,
    iptc_on: bool,
) -> MetadataPlan {
    let source_has_exif =
        !source.ifd0.is_empty() || !source.exif.is_empty() || !source.gps.is_empty();
    let has_exif = exif_on && source_has_exif;
    let tags = if has_exif {
        rewrite(source, width, height, now)
    } else {
        MetadataTags::default()
    };
    let icc = if !icc_on {
        IccOrigin::Off
    } else {
        match (&source.icc, source.kind) {
            (Some(_), SourceKind::Raw) => IccOrigin::BuiltinSrgb,
            (Some(_), SourceKind::Tiff) => IccOrigin::Source,
            (None, _) => IccOrigin::None,
        }
    };
    MetadataPlan {
        tags,
        has_exif,
        icc,
        icc_bytes: if icc_on { source.icc.clone() } else { None },
        has_xmp: xmp_on && source.xmp.is_some(),
        xmp_bytes: if xmp_on { source.xmp.clone() } else { None },
        has_iptc: iptc_on && source.iptc.is_some(),
        iptc_bytes: if iptc_on { source.iptc.clone() } else { None },
    }
}

/// One IFD0 entry for a raw byte block (ICC, XMP or IPTC): `field_type`
/// `BYTE` for XMP (some writers use `LONG` for IPTC, but this project always
/// writes `UNDEFINED`, matching the ICC profile).
fn raw_block_entry(tag: u16, field_type: u16, bytes: &[u8]) -> TagEntry {
    TagEntry {
        tag,
        field_type,
        count: bytes.len() as u32,
        value: bytes.to_vec(),
    }
}

/// TIFF tag 34675, `UNDEFINED`.
pub(crate) fn icc_tag_entry(bytes: &[u8]) -> TagEntry {
    raw_block_entry(TAG_ICC_PROFILE, FIELD_TYPE_UNDEFINED, bytes)
}

/// TIFF tag 700, `BYTE`.
pub(crate) fn xmp_tag_entry(bytes: &[u8]) -> TagEntry {
    raw_block_entry(TAG_XMP, FIELD_TYPE_BYTE, bytes)
}

/// TIFF tag 33723, `UNDEFINED`.
pub(crate) fn iptc_tag_entry(bytes: &[u8]) -> TagEntry {
    raw_block_entry(TAG_IPTC, FIELD_TYPE_UNDEFINED, bytes)
}

/// The namespace header (`http://ns.adobe.com/xap/1.0/\0`) an XMP packet is
/// prefixed with in a JPEG APP1 segment; TIFF and PNG carry the packet bare.
pub(crate) const XMP_APP1_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

/// The IPTC block wrapped as a Photoshop `8BIM` 0x0404 image resource under
/// a `Photoshop 3.0\0` APP13 segment: a 4-byte signature, the resource ID, an
/// empty Pascal name padded to two bytes, a 4-byte big-endian size and the
/// data itself, padded to an even length.
pub(crate) fn photoshop_iptc_segment(iptc: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(iptc.len() + 26);
    out.extend_from_slice(b"Photoshop 3.0\0");
    out.extend_from_slice(b"8BIM");
    out.extend_from_slice(&0x0404u16.to_be_bytes());
    out.extend_from_slice(&[0u8, 0u8]); // empty Pascal name, padded to 2 bytes
    out.extend_from_slice(&(iptc.len() as u32).to_be_bytes());
    out.extend_from_slice(iptc);
    if iptc.len() % 2 == 1 {
        out.push(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::super::grouping::parse_exif_datetime;
    use super::*;

    #[test]
    fn exif_to_metadata_maps_populated_fields_onto_the_right_tags_and_types() {
        let mut exif = Exif {
            orientation: Some(1),
            artist: Some("Ansel".to_string()),
            copyright: Some("(c) Ansel".to_string()),
            exposure_time: Some(Rational { n: 1, d: 200 }),
            fnumber: Some(Rational { n: 28, d: 10 }),
            brightness_value: Some(SRational { n: -3, d: 1 }),
            iso_speed_ratings: Some(400),
            date_time_original: Some("2024:01:01 12:00:00".to_string()),
            lens_spec: Some([
                Rational { n: 24, d: 1 },
                Rational { n: 70, d: 1 },
                Rational { n: 28, d: 10 },
                Rational { n: 40, d: 10 },
            ]),
            timezone_offset: Some(vec![-5]),
            ..Exif::default()
        };
        exif.gps = Some(ExifGPS {
            gps_latitude_ref: Some("N".to_string()),
            gps_latitude: Some([
                Rational { n: 37, d: 1 },
                Rational { n: 46, d: 1 },
                Rational { n: 30, d: 1 },
            ]),
            gps_version_id: Some([2, 3, 0, 0]),
            gps_altitude_ref: Some(0),
            gps_processing_method: Some(vec![0x41, 0x53, 0x43, 0x49, 0x49]),
            ..ExifGPS::default()
        });

        let metadata = exif_to_metadata(&exif, "Canon", "EOS R5");

        assert_eq!(
            metadata.ifd0,
            vec![
                exif_tag_entry(ExifTag::Make, &"Canon".to_string()),
                exif_tag_entry(ExifTag::Model, &"EOS R5".to_string()),
                exif_tag_entry(ExifTag::Orientation, &1u16),
                exif_tag_entry(ExifTag::Artist, &"Ansel".to_string()),
                exif_tag_entry(ExifTag::Copyright, &"(c) Ansel".to_string()),
            ]
        );
        assert!(metadata.exif.contains(&exif_tag_entry(
            ExifTag::ExposureTime,
            &Rational { n: 1, d: 200 }
        )));
        assert!(metadata.exif.contains(&exif_tag_entry(
            ExifTag::FNumber,
            &Rational { n: 28, d: 10 }
        )));
        assert!(metadata.exif.contains(&exif_tag_entry(
            ExifTag::BrightnessValue,
            &SRational { n: -3, d: 1 }
        )));
        assert!(
            metadata
                .exif
                .contains(&exif_tag_entry(ExifTag::ISOSpeedRatings, &400u16))
        );
        assert!(metadata.exif.contains(&exif_tag_entry(
            ExifTag::DateTimeOriginal,
            &"2024:01:01 12:00:00".to_string()
        )));
        let lens_spec_entry = metadata
            .exif
            .iter()
            .find(|entry| entry.tag == ExifTag::LensSpecification as u16)
            .expect("LensSpecification entry");
        assert_eq!(lens_spec_entry.field_type, FIELD_TYPE_RATIONAL);
        assert_eq!(lens_spec_entry.count, 4);
        assert_eq!(lens_spec_entry.value.len(), 32);
        let timezone_entry = metadata
            .exif
            .iter()
            .find(|entry| entry.tag == ExifTag::TimeZoneOffset as u16)
            .expect("TimeZoneOffset entry");
        assert_eq!(timezone_entry.field_type, FIELD_TYPE_SSHORT);
        assert_eq!(timezone_entry.value, (-5i16).to_le_bytes().to_vec());

        assert!(
            metadata
                .gps
                .contains(&gps_tag_entry(ExifGpsTag::GPSLatitudeRef, &"N".to_string()))
        );
        assert!(
            metadata
                .gps
                .contains(&gps_tag_entry(ExifGpsTag::GPSVersionID, &[2u8, 3, 0, 0]))
        );
        let processing_method_entry = metadata
            .gps
            .iter()
            .find(|entry| entry.tag == ExifGpsTag::GPSProcessingMethod as u16)
            .expect("GPSProcessingMethod entry");
        assert_eq!(processing_method_entry.field_type, FIELD_TYPE_UNDEFINED);
        assert_eq!(
            processing_method_entry.value,
            vec![0x41, 0x53, 0x43, 0x49, 0x49]
        );

        assert_eq!(metadata.kind, SourceKind::Raw);
        assert_eq!(metadata.icc, Some(BUILTIN_SRGB_ICC.to_vec()));
        assert_eq!(metadata.xmp, None);
        assert_eq!(metadata.iptc, None);
    }

    #[test]
    fn exif_to_metadata_over_an_empty_exif_yields_only_make_and_model() {
        let metadata = exif_to_metadata(&Exif::default(), "Nikon", "Z9");

        assert_eq!(
            metadata.ifd0,
            vec![
                exif_tag_entry(ExifTag::Make, &"Nikon".to_string()),
                exif_tag_entry(ExifTag::Model, &"Z9".to_string()),
            ]
        );
        assert!(metadata.exif.is_empty());
        assert!(metadata.gps.is_empty());
        assert_eq!(metadata.icc, Some(BUILTIN_SRGB_ICC.to_vec()));
    }

    #[test]
    fn builtin_srgb_profile_is_the_456_byte_saucecontrol_micro_profile() {
        assert_eq!(BUILTIN_SRGB_ICC.len(), 456);
        // ICC header: bytes 36..40 are the profile signature "acsp".
        assert_eq!(&BUILTIN_SRGB_ICC[36..40], b"acsp");
    }

    fn fixed_offset_hms(
        offset_hours: i32,
        y: i32,
        m: u32,
        d: u32,
        h: u32,
        mi: u32,
        s: u32,
    ) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(offset_hours * 3600)
            .unwrap()
            .with_ymd_and_hms(y, m, d, h, mi, s)
            .single()
            .unwrap()
    }

    #[test]
    fn rewrite_overrides_software_datetime_offset_and_dimensions_and_keeps_the_rest() {
        let source = SourceMetadata {
            ifd0: vec![
                exif_tag_entry(ExifTag::Make, &"Canon".to_string()),
                exif_tag_entry(ExifTag::Software, &"darktable 4.8".to_string()),
            ],
            exif: vec![
                exif_tag_entry(
                    ExifTag::DateTimeOriginal,
                    &"2024:01:01 12:00:00".to_string(),
                ),
                exif_tag_entry(ExifTag::ExposureTime, &Rational { n: 1, d: 200 }),
            ],
            gps: vec![gps_tag_entry(ExifGpsTag::GPSLatitudeRef, &"N".to_string())],
            icc: None,
            xmp: None,
            iptc: None,
            kind: SourceKind::Tiff,
            warnings: Vec::new(),
        };
        let now = fixed_offset_hms(2, 2026, 9, 29, 14, 30, 5);

        let tags = rewrite(&source, 640, 480, now);

        assert!(
            tags.ifd0
                .contains(&exif_tag_entry(ExifTag::Make, &"Canon".to_string())),
            "an untouched IFD0 entry must survive"
        );
        let software = tags
            .ifd0
            .iter()
            .find(|e| e.tag == ExifTag::Software as u16)
            .expect("Software entry");
        assert_eq!(
            String::from_utf8(software.value.clone()).unwrap(),
            format!("FocusForge {}\0", env!("CARGO_PKG_VERSION"))
        );
        let date_time = tags
            .ifd0
            .iter()
            .find(|e| e.tag == ExifTag::ModifyDate as u16)
            .expect("DateTime entry");
        assert_eq!(
            String::from_utf8(date_time.value.clone()).unwrap(),
            "2026:09:29 14:30:05\0"
        );

        let offset = tags
            .exif
            .iter()
            .find(|e| e.tag == ExifTag::OffsetTime as u16)
            .expect("OffsetTime entry");
        assert_eq!(String::from_utf8(offset.value.clone()).unwrap(), "+02:00\0");

        let width = tags
            .exif
            .iter()
            .find(|e| e.tag == ExifTag::ExifImageWidth as u16)
            .expect("ExifImageWidth entry");
        assert_eq!(width.value, 640u32.to_le_bytes().to_vec());
        let height = tags
            .exif
            .iter()
            .find(|e| e.tag == ExifTag::ExifImageHeight as u16)
            .expect("ExifImageHeight entry");
        assert_eq!(height.value, 480u32.to_le_bytes().to_vec());

        assert!(
            tags.exif.contains(&exif_tag_entry(
                ExifTag::DateTimeOriginal,
                &"2024:01:01 12:00:00".to_string()
            )),
            "DateTimeOriginal is capture time, never rewritten"
        );
        assert_eq!(tags.gps, source.gps);
    }

    #[test]
    fn exif_datetime_round_trips_through_parse_exif_datetime_across_a_leap_day_and_a_year_boundary()
    {
        for (y, m, d, h, mi, s) in [
            (2024, 2, 29, 23, 59, 58),
            (2025, 12, 31, 23, 59, 59),
            (2026, 1, 1, 0, 0, 1),
        ] {
            let now = fixed_offset_hms(0, y, m, d, h, mi, s);
            let text = exif_datetime(now);
            let parsed = parse_exif_datetime(&text).expect("a well-formed EXIF datetime");
            assert_eq!(parsed.epoch_seconds, now.timestamp(), "{text}");
        }
    }

    #[test]
    fn exif_blob_parses_back_through_the_reader_to_the_rewritten_entries() {
        let tags = MetadataTags {
            ifd0: vec![exif_tag_entry(
                ExifTag::Software,
                &"FocusForge 0.1.0".to_string(),
            )],
            exif: vec![
                exif_tag_entry(
                    ExifTag::DateTimeOriginal,
                    &"2024:06:15 09:30:00".to_string(),
                ),
                exif_tag_entry(ExifTag::ExposureTime, &Rational { n: 1, d: 200 }),
            ],
            gps: vec![gps_tag_entry(ExifGpsTag::GPSLatitudeRef, &"N".to_string())],
        };

        let blob = exif_blob(&tags);

        let read = crate::io::tiff::header::read_tiff_metadata(
            &blob.as_slice(),
            crate::io::tiff::header::ByteOrder::Little,
            8,
            std::path::Path::new("exif-blob"),
        );

        assert!(read.warnings.is_empty(), "warnings: {:?}", read.warnings);
        assert!(read.ifd0.contains(&tags.ifd0[0]));
        assert!(read.exif.contains(&tags.exif[0]));
        assert!(read.exif.contains(&tags.exif[1]));
        assert!(read.gps.contains(&tags.gps[0]));
    }

    fn populated_tiff_source() -> SourceMetadata {
        SourceMetadata {
            ifd0: vec![exif_tag_entry(ExifTag::Make, &"Canon".to_string())],
            exif: vec![exif_tag_entry(
                ExifTag::DateTimeOriginal,
                &"2024:01:01 12:00:00".to_string(),
            )],
            gps: Vec::new(),
            icc: Some(vec![1, 2, 3]),
            xmp: Some(b"<x:xmpmeta/>".to_vec()),
            iptc: Some(vec![4, 5, 6]),
            kind: SourceKind::Tiff,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn resolve_plan_with_exif_off_never_rewrites_even_a_populated_source() {
        let now = fixed_offset_hms(0, 2026, 1, 1, 0, 0, 0);
        let plan = resolve_plan(
            &populated_tiff_source(),
            10,
            20,
            now,
            false,
            false,
            false,
            false,
        );
        assert!(!plan.has_exif);
        assert!(plan.tags.ifd0.is_empty());
        assert!(plan.tags.exif.is_empty());
        assert_eq!(plan.icc, IccOrigin::Off);
        assert!(plan.icc_bytes.is_none());
        assert!(!plan.has_xmp);
        assert!(!plan.has_iptc);
    }

    #[test]
    fn resolve_plan_with_exif_on_and_an_empty_source_carries_nothing() {
        let now = fixed_offset_hms(0, 2026, 1, 1, 0, 0, 0);
        let plan = resolve_plan(
            &SourceMetadata::default(),
            10,
            20,
            now,
            true,
            false,
            false,
            false,
        );
        assert!(
            !plan.has_exif,
            "an empty source has nothing for `rewrite` to attach to"
        );
        assert!(plan.tags.ifd0.is_empty());
    }

    #[test]
    fn resolve_plan_with_exif_on_and_a_populated_source_rewrites_it() {
        let now = fixed_offset_hms(0, 2026, 1, 1, 0, 0, 0);
        let plan = resolve_plan(
            &populated_tiff_source(),
            10,
            20,
            now,
            true,
            false,
            false,
            false,
        );
        assert!(plan.has_exif);
        assert!(
            plan.tags
                .ifd0
                .contains(&exif_tag_entry(ExifTag::Make, &"Canon".to_string())),
            "source identity survives"
        );
        assert!(
            plan.tags
                .ifd0
                .iter()
                .any(|e| e.tag == ExifTag::Software as u16),
            "rewrite adds Software"
        );
    }

    #[test]
    fn resolve_plan_icc_origin_follows_the_switch_and_the_source_kind() {
        let now = fixed_offset_hms(0, 2026, 1, 1, 0, 0, 0);
        let tiff_with_profile = populated_tiff_source();
        let plan = resolve_plan(&tiff_with_profile, 1, 1, now, false, true, false, false);
        assert_eq!(plan.icc, IccOrigin::Source);
        assert_eq!(plan.icc_bytes, Some(vec![1, 2, 3]));

        let tiff_without_profile = SourceMetadata {
            icc: None,
            ..populated_tiff_source()
        };
        let plan = resolve_plan(&tiff_without_profile, 1, 1, now, false, true, false, false);
        assert_eq!(plan.icc, IccOrigin::None);
        assert!(plan.icc_bytes.is_none());

        let raw = SourceMetadata {
            kind: SourceKind::Raw,
            icc: Some(vec![9]),
            ..populated_tiff_source()
        };
        let plan = resolve_plan(&raw, 1, 1, now, false, true, false, false);
        assert_eq!(plan.icc, IccOrigin::BuiltinSrgb);

        let plan = resolve_plan(&tiff_with_profile, 1, 1, now, false, false, false, false);
        assert_eq!(plan.icc, IccOrigin::Off);
        assert!(plan.icc_bytes.is_none(), "off carries no bytes either");
    }

    #[test]
    fn resolve_plan_xmp_and_iptc_are_absent_when_the_switch_is_on_but_the_source_has_none() {
        let now = fixed_offset_hms(0, 2026, 1, 1, 0, 0, 0);
        let raw = SourceMetadata {
            kind: SourceKind::Raw,
            xmp: None,
            iptc: None,
            ..populated_tiff_source()
        };
        let plan = resolve_plan(&raw, 1, 1, now, false, false, true, true);
        assert!(!plan.has_xmp);
        assert!(plan.xmp_bytes.is_none());
        assert!(!plan.has_iptc);
        assert!(plan.iptc_bytes.is_none());

        let plan = resolve_plan(
            &populated_tiff_source(),
            1,
            1,
            now,
            false,
            false,
            true,
            true,
        );
        assert!(plan.has_xmp);
        assert_eq!(plan.xmp_bytes, Some(b"<x:xmpmeta/>".to_vec()));
        assert!(plan.has_iptc);
        assert_eq!(plan.iptc_bytes, Some(vec![4, 5, 6]));
    }

    #[test]
    fn icc_xmp_iptc_tag_entries_carry_the_right_tag_and_field_type() {
        let icc = icc_tag_entry(&[1, 2]);
        assert_eq!(icc.tag, TAG_ICC_PROFILE);
        assert_eq!(icc.field_type, FIELD_TYPE_UNDEFINED);
        let xmp = xmp_tag_entry(&[3, 4]);
        assert_eq!(xmp.tag, TAG_XMP);
        assert_eq!(xmp.field_type, FIELD_TYPE_BYTE);
        let iptc = iptc_tag_entry(&[5, 6]);
        assert_eq!(iptc.tag, TAG_IPTC);
        assert_eq!(iptc.field_type, FIELD_TYPE_UNDEFINED);
    }

    #[test]
    fn photoshop_iptc_segment_pads_odd_length_data_to_even() {
        let segment = photoshop_iptc_segment(&[1, 2, 3]);
        assert!(segment.starts_with(b"Photoshop 3.0\08BIM"));
        assert_eq!(segment.len() % 2, 0, "odd-length IPTC data must be padded");
        assert!(segment.ends_with(&[1, 2, 3, 0]));
    }
}
