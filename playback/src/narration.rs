use std::collections::HashMap;

use crate::decoder::{AudioDecoder, AudioPacket, AudioPacketPosition, DecoderResult};

/// Logical part of a Spotify DJ item currently audible to the listener.
///
/// `Music` is included deliberately: it tells consumers when the generated
/// voice has ended so they can restore the actual track title/artwork.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NarrationPhase {
    Intro,
    Music,
    Outro,
}

/// Narration fields carried by Spotify DJ context tracks.
///
/// The values are deliberately kept independent from the generated protocol
/// enums.  Context metadata is untrusted text and Spotify has changed the
/// spelling of these values between clients, so conversion happens at the
/// boundary where the TTS request is built.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NarrationMetadata {
    pub intro_ssml: Option<String>,
    pub jump_ssml: Option<String>,
    pub outro_ssml: Option<String>,
    pub intro_image: Option<String>,
    pub jump_image: Option<String>,
    pub outro_image: Option<String>,
    pub language: String,
    pub voice: u32,
    pub provider: u32,
    pub intro_voice: u32,
    pub intro_provider: u32,
    pub jump_voice: u32,
    pub jump_provider: u32,
    pub outro_voice: u32,
    pub outro_provider: u32,
    pub intro_loudness_db: Option<f64>,
    pub intro_true_peak_db: Option<f64>,
    pub jump_loudness_db: Option<f64>,
    pub jump_true_peak_db: Option<f64>,
    pub outro_loudness_db: Option<f64>,
    pub outro_true_peak_db: Option<f64>,
    /// Backwards-compatible shared values used by older context producers.
    pub loudness_db: Option<f64>,
    pub true_peak: Option<f64>,
}

impl NarrationMetadata {
    pub fn from_track_metadata(metadata: &HashMap<String, String>) -> Option<Self> {
        let intro_ssml = non_empty(metadata.get("narration.intro.ssml"));
        let jump_ssml = non_empty(metadata.get("narration.jump.ssml"));
        let outro_ssml = non_empty(metadata.get("narration.outro.ssml"));
        if intro_ssml.is_none() && jump_ssml.is_none() && outro_ssml.is_none() {
            return None;
        }

        let language = metadata
            .get("narration.language")
            .or_else(|| metadata.get("language"))
            .cloned()
            .unwrap_or_else(|| "en-US".to_string());
        let voice = parse_segment_voice(metadata, "narration", 1);
        let provider = parse_segment_provider(metadata, "narration", 6);

        let shared_loudness_db = parse_float(metadata.get("narration.loudness"));
        let shared_true_peak_db = parse_float(metadata.get("narration.true_peak"));

        Some(Self {
            intro_ssml,
            jump_ssml,
            outro_ssml,
            intro_image: parse_image(metadata.get("narration.intro.image")),
            jump_image: parse_image(metadata.get("narration.jump.image")),
            outro_image: parse_image(metadata.get("narration.outro.image")),
            language,
            voice,
            provider,
            intro_voice: parse_segment_voice(metadata, "narration.intro", voice),
            intro_provider: parse_segment_provider(metadata, "narration.intro", provider),
            jump_voice: parse_segment_voice(metadata, "narration.jump", voice),
            jump_provider: parse_segment_provider(metadata, "narration.jump", provider),
            outro_voice: parse_segment_voice(metadata, "narration.outro", voice),
            outro_provider: parse_segment_provider(metadata, "narration.outro", provider),
            intro_loudness_db: parse_segment_float(metadata, "narration.intro", "loudness")
                .or(shared_loudness_db),
            intro_true_peak_db: parse_segment_float(metadata, "narration.intro", "true_peak")
                .or(shared_true_peak_db),
            jump_loudness_db: parse_segment_float(metadata, "narration.jump", "loudness")
                .or(shared_loudness_db),
            jump_true_peak_db: parse_segment_float(metadata, "narration.jump", "true_peak")
                .or(shared_true_peak_db),
            outro_loudness_db: parse_segment_float(metadata, "narration.outro", "loudness")
                .or(shared_loudness_db),
            outro_true_peak_db: parse_segment_float(metadata, "narration.outro", "true_peak")
                .or(shared_true_peak_db),
            loudness_db: shared_loudness_db,
            true_peak: shared_true_peak_db,
        })
    }

    pub fn has_intro_or_outro(&self) -> bool {
        self.intro_ssml.is_some() || self.outro_ssml.is_some()
    }

    pub(crate) fn artwork(&self) -> NarrationArtwork {
        NarrationArtwork {
            intro: self.intro_image.clone(),
            outro: self.outro_image.clone(),
        }
    }
}

fn non_empty(value: Option<&String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty()).cloned()
}

/// Convert the image forms used in DJ context metadata into URLs understood by
/// the UI image loader. Spotify commonly sends `spotify:image:<id>`; a few
/// context versions already provide an HTTPS URL, so keep those unchanged.
fn parse_image(value: Option<&String>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }

    if value.starts_with("https://") || value.starts_with("http://") {
        return Some(value.to_string());
    }

    let image_id = value
        .strip_prefix("spotify:image:")
        .or_else(|| value.strip_prefix("image:"))
        .unwrap_or(value)
        .trim();
    (!image_id.is_empty()).then(|| format!("https://i.scdn.co/image/{image_id}"))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NarrationArtwork {
    pub(crate) intro: Option<String>,
    pub(crate) outro: Option<String>,
}

impl NarrationArtwork {
    pub(crate) fn image_for_phase(&self, phase: NarrationPhase) -> Option<&str> {
        match phase {
            NarrationPhase::Intro => self.intro.as_deref(),
            NarrationPhase::Outro => self.outro.as_deref(),
            NarrationPhase::Music => None,
        }
    }
}

fn parse_float(value: Option<&String>) -> Option<f64> {
    value.and_then(|value| value.parse::<f64>().ok().filter(|value| value.is_finite()))
}

fn parse_segment_float(
    metadata: &HashMap<String, String>,
    prefix: &str,
    field: &str,
) -> Option<f64> {
    parse_float(metadata.get(&format!("{prefix}.{field}")))
}

fn parse_voice(value: &str) -> u32 {
    parse_enum(value, "voice", 40)
}

fn parse_provider(value: &str) -> u32 {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "cloud_tts" | "cloud-tts" | "cloud tts" => 1,
        "readspeaker" => 2,
        "polly" => 3,
        "well_said" | "well-said" | "well said" => 4,
        "sonantic_deprecated" | "sonantic-deprecated" => 5,
        "sonantic_fast" | "sonantic-fast" => 6,
        _ => normalized.parse::<u32>().unwrap_or_default(),
    }
}

fn parse_enum(value: &str, prefix: &str, max: u32) -> u32 {
    let normalized = value.trim().to_ascii_lowercase();
    let number = normalized
        .strip_prefix(prefix)
        .unwrap_or(&normalized)
        .parse::<u32>()
        .unwrap_or_default();
    if number <= max { number } else { 0 }
}

fn parse_segment_voice(metadata: &HashMap<String, String>, prefix: &str, fallback: u32) -> u32 {
    metadata
        .get(&format!("{prefix}.voice"))
        .map(|value| parse_voice(value))
        .filter(|value| *value != 0)
        .unwrap_or(fallback)
}

fn parse_segment_provider(metadata: &HashMap<String, String>, prefix: &str, fallback: u32) -> u32 {
    metadata
        .get(&format!("{prefix}.tts_provider"))
        .map(|value| parse_provider(value))
        .filter(|value| *value != 0)
        .unwrap_or(fallback)
}

type Decoder = Box<dyn AudioDecoder + Send>;

const NARRATION_LOUDNESS_TARGET_DB: f64 = -14.0;

/// Convert Spotify's narration loudness metadata into a linear PCM gain.
///
/// DJ clips carry their own LUFS/true-peak values, unlike regular tracks whose
/// values are read from the audio container.  Keep this adjustment local to
/// the narration decoder so the music stream and its existing normalisation
/// remain untouched.
pub(crate) fn narration_gain(
    loudness_db: Option<f64>,
    true_peak_db: Option<f64>,
    pregain_db: f64,
) -> Option<f64> {
    let loudness_db = loudness_db.filter(|value| value.is_finite())?;
    let true_peak_db = true_peak_db.filter(|value| value.is_finite())?;
    let gain_db = NARRATION_LOUDNESS_TARGET_DB - loudness_db + pregain_db;
    let mut gain = 10_f64.powf(gain_db / 20.0);

    let true_peak = 10_f64.powf(true_peak_db / 20.0);
    if true_peak.is_finite() && true_peak > 0.0 {
        gain = gain.min(1.0 / true_peak);
    }

    gain.is_finite().then_some(gain.max(0.0))
}

struct GainDecoder {
    inner: Decoder,
    gain: f64,
}

impl AudioDecoder for GainDecoder {
    fn seek(&mut self, position_ms: u32) -> Result<u32, crate::decoder::DecoderError> {
        self.inner.seek(position_ms)
    }

    fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
        let Some((position, mut packet)) = self.inner.next_packet()? else {
            return Ok(None);
        };

        if let AudioPacket::Samples(samples) = &mut packet {
            for sample in samples {
                *sample *= self.gain;
            }
        }

        Ok(Some((position, packet)))
    }

    fn narration_phase(&self, position_ms: u32) -> Option<NarrationPhase> {
        self.inner.narration_phase(position_ms)
    }
}

pub(crate) fn with_narration_gain(
    decoder: Decoder,
    loudness_db: Option<f64>,
    true_peak_db: Option<f64>,
    pregain_db: f64,
) -> Decoder {
    let Some(gain) = narration_gain(loudness_db, true_peak_db, pregain_db) else {
        return decoder;
    };

    if (gain - 1.0).abs() < f64::EPSILON {
        decoder
    } else {
        Box::new(GainDecoder {
            inner: decoder,
            gain,
        })
    }
}

struct Segment {
    decoder: Decoder,
    duration_ms: u32,
    phase: NarrationPhase,
}

/// Presents intro, main track and outro as one decoder stream.
///
/// Segment boundaries are invisible to the sink: packet timestamps are shifted
/// by the accumulated duration and end-of-stream only occurs after the last
/// segment.  Seeking into the composed stream skips the intro automatically.
pub(crate) struct NarrationDecoder {
    segments: Vec<Segment>,
    offsets: Vec<u32>,
    current: usize,
    total_duration_ms: u32,
}

impl NarrationDecoder {
    pub(crate) fn new(
        intro: Option<(Decoder, u32)>,
        main: Decoder,
        main_duration_ms: u32,
        outro: Option<(Decoder, u32)>,
    ) -> (Decoder, u32) {
        let mut segments = Vec::with_capacity(3);
        if let Some((decoder, duration_ms)) = intro {
            segments.push(Segment {
                decoder,
                duration_ms,
                phase: NarrationPhase::Intro,
            });
        }
        segments.push(Segment {
            decoder: main,
            duration_ms: main_duration_ms,
            phase: NarrationPhase::Music,
        });
        if let Some((decoder, duration_ms)) = outro {
            segments.push(Segment {
                decoder,
                duration_ms,
                phase: NarrationPhase::Outro,
            });
        }

        let mut total_duration_ms: u32 = 0;
        let offsets = segments
            .iter()
            .map(|segment| {
                let offset = total_duration_ms;
                total_duration_ms = total_duration_ms.saturating_add(segment.duration_ms);
                offset
            })
            .collect();

        (
            Box::new(Self {
                segments,
                offsets,
                current: 0,
                total_duration_ms,
            }),
            total_duration_ms,
        )
    }

    fn phase_at(&self, position_ms: u32) -> Option<NarrationPhase> {
        let target = position_ms.min(self.total_duration_ms);
        self.segments
            .iter()
            .enumerate()
            .find(|(index, segment)| {
                let end = self.offsets[*index].saturating_add(segment.duration_ms);
                target < end || *index == self.segments.len() - 1
            })
            .map(|(_, segment)| segment.phase)
    }
}

impl AudioDecoder for NarrationDecoder {
    fn seek(&mut self, position_ms: u32) -> Result<u32, crate::decoder::DecoderError> {
        let target = position_ms.min(self.total_duration_ms);
        let index = self
            .segments
            .iter()
            .enumerate()
            .find(|(index, segment)| {
                let end = self.offsets[*index].saturating_add(segment.duration_ms);
                target < end || *index == self.segments.len() - 1
            })
            .map(|(index, _)| index)
            .unwrap_or(0);

        self.current = index;
        let local_position = target.saturating_sub(self.offsets[index]);
        let actual = self.segments[index].decoder.seek(local_position)?;
        Ok(self.offsets[index].saturating_add(actual))
    }

    fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
        loop {
            let current = self.current;
            let Some(offset) = self.offsets.get(current).copied() else {
                return Ok(None);
            };
            let Some(segment) = self.segments.get_mut(current) else {
                return Ok(None);
            };

            match segment.decoder.next_packet()? {
                Some((position, packet)) => {
                    return Ok(Some((
                        AudioPacketPosition {
                            position_ms: offset.saturating_add(position.position_ms),
                            skipped: position.skipped,
                        },
                        packet,
                    )));
                }
                None => self.current += 1,
            }
        }
    }

    fn narration_phase(&self, position_ms: u32) -> Option<NarrationPhase> {
        self.phase_at(position_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::{NarrationMetadata, narration_gain};
    use std::collections::HashMap;

    #[test]
    fn parses_dj_metadata_without_requiring_every_optional_field() {
        let metadata = HashMap::from([
            (
                "narration.intro.ssml".to_string(),
                "<speak>Hi</speak>".to_string(),
            ),
            ("narration.voice".to_string(), "VOICE7".to_string()),
            (
                "narration.tts_provider".to_string(),
                "sonantic_fast".to_string(),
            ),
        ]);

        let parsed = NarrationMetadata::from_track_metadata(&metadata).unwrap();
        assert_eq!(parsed.voice, 7);
        assert_eq!(parsed.provider, 6);
        assert_eq!(parsed.intro_voice, 7);
        assert_eq!(parsed.intro_provider, 6);
        assert!(parsed.outro_ssml.is_none());
    }

    #[test]
    fn parses_narration_images_as_cdn_urls() {
        let metadata = HashMap::from([
            (
                "narration.intro.ssml".to_string(),
                "<speak>Hi</speak>".to_string(),
            ),
            (
                "narration.intro.image".to_string(),
                "spotify:image:abc123".to_string(),
            ),
            (
                "narration.outro.image".to_string(),
                "https://example.test/dj.png".to_string(),
            ),
        ]);

        let parsed = NarrationMetadata::from_track_metadata(&metadata).unwrap();
        assert_eq!(
            parsed.intro_image.as_deref(),
            Some("https://i.scdn.co/image/abc123")
        );
        assert_eq!(
            parsed.outro_image.as_deref(),
            Some("https://example.test/dj.png")
        );
    }

    #[test]
    fn prefers_per_segment_voice_and_provider() {
        let metadata = HashMap::from([
            (
                "narration.intro.ssml".to_string(),
                "<speak>Hi</speak>".to_string(),
            ),
            ("narration.intro.voice".to_string(), "VOICE7".to_string()),
            (
                "narration.intro.tts_provider".to_string(),
                "sonantic_fast".to_string(),
            ),
        ]);

        let parsed = NarrationMetadata::from_track_metadata(&metadata).unwrap();
        assert_eq!(parsed.intro_voice, 7);
        assert_eq!(parsed.intro_provider, 6);
        assert_eq!(parsed.outro_voice, 1);
        assert_eq!(parsed.outro_provider, 6);
    }

    #[test]
    fn parses_per_segment_loudness_and_true_peak() {
        let metadata = HashMap::from([
            (
                "narration.intro.ssml".to_string(),
                "<speak>Hi</speak>".to_string(),
            ),
            ("narration.intro.loudness".to_string(), "-16.5".to_string()),
            ("narration.intro.true_peak".to_string(), "-3.0".to_string()),
            ("narration.jump.loudness".to_string(), "-18".to_string()),
            ("narration.jump.true_peak".to_string(), "-2".to_string()),
        ]);

        let parsed = NarrationMetadata::from_track_metadata(&metadata).unwrap();
        assert_eq!(parsed.intro_loudness_db, Some(-16.5));
        assert_eq!(parsed.intro_true_peak_db, Some(-3.0));
        assert_eq!(parsed.jump_loudness_db, Some(-18.0));
        assert_eq!(parsed.jump_true_peak_db, Some(-2.0));
        assert_eq!(parsed.outro_loudness_db, None);
    }

    #[test]
    fn narration_gain_targets_spotify_loudness_without_clipping() {
        let gain = narration_gain(Some(-16.0), Some(-3.0), 0.0).unwrap();
        assert!((gain - 10_f64.powf(2.0 / 20.0)).abs() < 1e-12);

        let clipped = narration_gain(Some(-30.0), Some(-1.0), 0.0).unwrap();
        assert!((clipped - 10_f64.powf(1.0 / 20.0)).abs() < 1e-12);
        assert!(narration_gain(Some(-16.0), None, 0.0).is_none());
    }

    #[test]
    fn ignores_empty_prompts() {
        let metadata = HashMap::from([("narration.intro.ssml".to_string(), "  ".to_string())]);
        assert!(NarrationMetadata::from_track_metadata(&metadata).is_none());
    }
}
