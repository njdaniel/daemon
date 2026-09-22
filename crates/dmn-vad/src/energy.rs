//! Energy VAD: frame RMS in dBFS against an adaptive noise floor.

use dmn_audio::FRAME_LEN;

/// One speech / non-speech decision per 20 ms frame. Implementations keep their own state
/// (noise floor, hangover); a Silero implementation replaces [`EnergyVad`] in M2.
pub trait Vad: Send {
    /// Classify one frame of [`FRAME_LEN`] samples at 16 kHz.
    fn is_speech(&mut self, frame: &[f32; FRAME_LEN]) -> bool;
    /// Forget all adaptive state (a source was reconnected, for example).
    fn reset(&mut self);
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnergyVadConfig {
    /// A frame is speech when its level exceeds the noise floor by this much.
    pub margin_db: f32,
    /// ...and exceeds this absolute level, so digital silence (floor at -100 dBFS) never turns
    /// low-level hiss into "speech".
    pub min_threshold_dbfs: f32,
    /// Frames of speech kept after the last energetic frame, so a single quiet frame inside a
    /// word never reads as silence. Short, because the segmenter's 300 / 700 ms timers already
    /// bridge real gaps and every hangover frame delays the utterance end by 20 ms.
    pub hangover_frames: u32,
    /// Where the floor starts. A prior, not the first frame: a stream that opens mid-sentence
    /// (joining a call while the interviewer talks) must not learn speech as its floor and lose
    /// the first utterance.
    pub floor_prior_dbfs: f32,
    /// How fast the floor rises toward non-speech frames, in dB per second. It falls fast (it
    /// closes `floor_fall` of the gap per frame toward quieter frames) and rises slowly, so a
    /// noisy room is learned within seconds while a pause between words does not drag the floor
    /// up to speech.
    pub floor_rise_db_per_s: f32,
    /// How fast the floor rises while frames read as speech, in dB per second. Slower than
    /// `floor_rise_db_per_s`, so 12 s of talk moves it only 6 dB, but not zero, so a room louder
    /// than the first threshold (a fan at -40 dBFS) is still learned instead of reading as
    /// endless speech.
    pub floor_leak_db_per_s: f32,
    /// Fraction of the gap closed per frame when the level is below the floor.
    pub floor_fall: f32,
}

impl Default for EnergyVadConfig {
    fn default() -> Self {
        Self {
            margin_db: 15.0,
            min_threshold_dbfs: -50.0,
            hangover_frames: 2,
            floor_prior_dbfs: -60.0,
            floor_rise_db_per_s: 1.0,
            floor_leak_db_per_s: 0.5,
            floor_fall: 0.3,
        }
    }
}

/// RMS level of a frame in dBFS, clamped to -100 for digital silence.
pub fn frame_dbfs(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return -100.0;
    }
    let mean_sq = frame.iter().map(|x| x * x).sum::<f32>() / frame.len() as f32;
    if mean_sq <= 1e-10 {
        return -100.0;
    }
    (10.0 * mean_sq.log10()).max(-100.0)
}

#[derive(Debug, Clone)]
pub struct EnergyVad {
    cfg: EnergyVadConfig,
    floor_db: Option<f32>,
    hang: u32,
    speaking: bool,
}

impl EnergyVad {
    pub fn new(cfg: EnergyVadConfig) -> Self {
        Self {
            cfg,
            floor_db: None,
            hang: 0,
            speaking: false,
        }
    }

    /// Current noise-floor estimate (dBFS), once at least one frame has been seen.
    pub fn floor_db(&self) -> Option<f32> {
        self.floor_db
    }

    fn threshold(&self, floor: f32) -> f32 {
        (floor + self.cfg.margin_db).max(self.cfg.min_threshold_dbfs)
    }
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self::new(EnergyVadConfig::default())
    }
}

impl Vad for EnergyVad {
    fn is_speech(&mut self, frame: &[f32; FRAME_LEN]) -> bool {
        let level = frame_dbfs(frame);
        let floor = *self.floor_db.get_or_insert(self.cfg.floor_prior_dbfs);
        let energetic = level > self.threshold(floor);

        // Fall fast toward quieter frames; rise slowly, and more slowly still during speech.
        let rise = if energetic {
            self.cfg.floor_leak_db_per_s
        } else {
            self.cfg.floor_rise_db_per_s
        } * 0.02;
        let next = if level < floor {
            floor + (level - floor) * self.cfg.floor_fall
        } else {
            (floor + rise).min(level)
        };
        self.floor_db = Some(next);

        if energetic {
            self.speaking = true;
            self.hang = self.cfg.hangover_frames;
        } else if self.speaking {
            if self.hang > 0 {
                self.hang -= 1;
            } else {
                self.speaking = false;
            }
        }
        self.speaking
    }

    fn reset(&mut self) {
        self.floor_db = None;
        self.hang = 0;
        self.speaking = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn tone(dbfs: f32) -> [f32; FRAME_LEN] {
        let amp = 10f32.powf(dbfs / 20.0) * std::f32::consts::SQRT_2;
        std::array::from_fn(|i| {
            amp * (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 16000.0).sin()
        })
    }

    #[test]
    fn dbfs_of_a_sine_matches_its_rms_and_silence_clamps() {
        assert!((frame_dbfs(&tone(-20.0)) + 20.0).abs() < 0.1);
        assert_eq!(frame_dbfs(&[0.0; FRAME_LEN]), -100.0);
    }

    #[test]
    fn speech_over_the_floor_is_detected_and_hangover_bridges_one_quiet_frame() {
        let mut vad = EnergyVad::default();
        for _ in 0..25 {
            assert!(!vad.is_speech(&tone(-60.0)), "noise floor is not speech");
        }
        assert!(vad.is_speech(&tone(-20.0)), "onset on the first loud frame");
        assert!(vad.is_speech(&tone(-60.0)), "hangover frame 1");
        assert!(
            vad.is_speech(&tone(-20.0)),
            "back to speech: the dip never read as silence"
        );
        assert!(vad.is_speech(&tone(-60.0)));
        assert!(vad.is_speech(&tone(-60.0)));
        assert!(!vad.is_speech(&tone(-60.0)), "hangover of 2 frames is over");
    }

    #[test]
    fn digital_silence_does_not_make_hiss_speech() {
        let mut vad = EnergyVad::default();
        for _ in 0..50 {
            vad.is_speech(&[0.0; FRAME_LEN]);
        }
        assert!(
            !vad.is_speech(&tone(-60.0)),
            "min threshold holds with a -100 dBFS floor"
        );
    }

    #[test]
    fn a_long_answer_does_not_raise_the_floor_and_a_noisy_room_is_learned() {
        let mut vad = EnergyVad::default();
        vad.is_speech(&tone(-60.0));
        for _ in 0..600 {
            assert!(vad.is_speech(&tone(-22.0)));
        }
        let floor = vad.floor_db().unwrap();
        assert!(
            floor < -53.0,
            "12 s of speech leaks the floor up only ~6 dB: {floor}"
        );

        // Room noise steps up to -48 dBFS (below the -45 threshold): the floor climbs toward it
        // at 1 dB/s and the noise never reads as speech.
        for _ in 0..3 {
            vad.is_speech(&tone(-60.0));
        }
        for _ in 0..1000 {
            assert!(!vad.is_speech(&tone(-48.0)));
        }
        let floor = vad.floor_db().unwrap();
        assert!(
            (floor + 48.0).abs() < 0.5,
            "floor learned the room: {floor}"
        );
    }

    #[test]
    fn a_stream_that_opens_mid_speech_detects_the_first_frame() {
        let mut vad = EnergyVad::default();
        assert!(
            vad.is_speech(&tone(-20.0)),
            "floor starts at the prior, not at the first frame"
        );
    }

    #[test]
    fn a_room_louder_than_the_first_threshold_is_learned_eventually() {
        let mut vad = EnergyVad::default();
        // A fan at -40 dBFS reads as speech at first (threshold -45). The leak lifts the floor
        // until the threshold passes the fan at a floor of -55: 5 dB at 0.5 dB/s is 10 s.
        let mut last_speech = 0;
        for i in 0..3000 {
            if vad.is_speech(&tone(-40.0)) {
                last_speech = i;
            }
        }
        assert!(
            (450..600).contains(&last_speech),
            "noise stopped reading as speech after ~10 s: frame {last_speech}"
        );
        assert!((vad.floor_db().unwrap() + 40.0).abs() < 0.5);
    }

    #[test]
    fn reset_forgets_the_floor() {
        let mut vad = EnergyVad::default();
        vad.is_speech(&tone(-60.0));
        vad.reset();
        assert!(vad.floor_db().is_none());
    }
}
