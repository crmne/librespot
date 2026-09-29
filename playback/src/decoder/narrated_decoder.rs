use super::{AudioDecoder, AudioPacket, AudioPacketPosition, DecoderResult};

type Decoder = Box<dyn AudioDecoder + Send>;

/// Keeps DJ speech in the same output stream as its song. Speech holds the
/// visible song position at zero (or its end) until the song actually runs.
pub struct NarratedDecoder {
    intro: Option<Decoder>,
    song: Decoder,
    outro: Option<Decoder>,
    song_end_ms: u32,
    phase: Phase,
}

#[derive(Clone, Copy)]
enum Phase {
    Intro,
    Song,
    Outro,
    Done,
}

impl NarratedDecoder {
    pub fn new(
        intro: Option<Decoder>,
        song: Decoder,
        outro: Option<Decoder>,
        song_end_ms: u32,
    ) -> Self {
        Self {
            phase: if intro.is_some() {
                Phase::Intro
            } else {
                Phase::Song
            },
            intro,
            song,
            outro,
            song_end_ms,
        }
    }
}

impl AudioDecoder for NarratedDecoder {
    fn seek(&mut self, position_ms: u32) -> DecoderResult<u32> {
        // A user seek goes straight to the requested song position.
        self.intro = None;
        self.phase = Phase::Song;
        self.song.seek(position_ms)
    }

    fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
        loop {
            match self.phase {
                Phase::Intro => {
                    if let Some(intro) = &mut self.intro
                        && let Some((_, packet)) = intro.next_packet()?
                    {
                        return Ok(Some((
                            AudioPacketPosition {
                                position_ms: 0,
                                skipped: false,
                            },
                            packet,
                        )));
                    }
                    self.intro = None;
                    self.phase = Phase::Song;
                }
                Phase::Song => {
                    if let Some(packet) = self.song.next_packet()? {
                        return Ok(Some(packet));
                    }
                    self.phase = Phase::Outro;
                }
                Phase::Outro => {
                    if let Some(outro) = &mut self.outro
                        && let Some((_, packet)) = outro.next_packet()?
                    {
                        return Ok(Some((
                            AudioPacketPosition {
                                position_ms: self.song_end_ms,
                                skipped: false,
                            },
                            packet,
                        )));
                    }
                    self.outro = None;
                    self.phase = Phase::Done;
                }
                Phase::Done => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PacketOnce(Option<(u32, f64)>);

    impl AudioDecoder for PacketOnce {
        fn seek(&mut self, position_ms: u32) -> DecoderResult<u32> {
            Ok(position_ms)
        }

        fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
            Ok(self.0.take().map(|(position_ms, sample)| {
                (
                    AudioPacketPosition {
                        position_ms,
                        skipped: false,
                    },
                    AudioPacket::Samples(vec![sample]),
                )
            }))
        }
    }

    #[test]
    fn spoken_segments_surround_the_song_without_advancing_song_time() {
        let mut decoder = NarratedDecoder::new(
            Some(Box::new(PacketOnce(Some((300, 1.0))))),
            Box::new(PacketOnce(Some((420, 2.0)))),
            Some(Box::new(PacketOnce(Some((500, 3.0))))),
            1000,
        );
        let mut seen = Vec::new();
        while let Some((position, packet)) = decoder.next_packet().unwrap() {
            seen.push((position.position_ms, packet.samples().unwrap()[0]));
        }
        assert_eq!(seen, [(0, 1.0), (420, 2.0), (1000, 3.0)]);
    }

    #[test]
    fn seeking_skips_the_unheard_intro() {
        let mut decoder = NarratedDecoder::new(
            Some(Box::new(PacketOnce(Some((300, 1.0))))),
            Box::new(PacketOnce(Some((420, 2.0)))),
            None,
            1000,
        );
        assert_eq!(decoder.seek(420).unwrap(), 420);
        let (position, packet) = decoder.next_packet().unwrap().unwrap();
        assert_eq!(position.position_ms, 420);
        assert_eq!(packet.samples().unwrap(), [2.0]);
    }
}
