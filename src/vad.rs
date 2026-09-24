//! Splits a live 16 kHz stream into spoken phrases.
//!
//! Whisper transcribes whole clips, not streams, so dictation works phrase by
//! phrase: an energy detector with an adaptive noise floor notices speech,
//! keeps a little audio from before it started, and closes the phrase after a
//! pause. The adaptive floor means a noisy laptop fan or a quiet USB mic both
//! work without calibration.

use crate::audio::{SAMPLE_RATE, rms};
use crate::config::AudioConfig;

const FRAME: usize = (SAMPLE_RATE as usize) * 30 / 1000; // 30 ms
const PRE_ROLL_FRAMES: usize = 10; // 300 ms kept from before speech starts
const START_FRAMES: usize = 3; // 90 ms of loudness before we call it speech
const MIN_FLOOR: f32 = 0.004; // -48 dBFS: quieter than this is never speech
const WARMUP_FRAMES: usize = 10; // learn the room quickly for the first 300 ms
const NO_START_FRAMES: usize = 5; // and ignore the microphone's opening burst
const RECENT_FRAMES: usize = 50; // 1.5 s: speech always has a gap in this long

pub enum VadEvent {
    SpeechStarted,
    /// Audio so far in the phrase, for a provisional transcript.
    Partial(Vec<f32>),
    /// A finished phrase.
    Phrase(Vec<f32>),
}

pub struct Segmenter {
    pending: Vec<f32>,
    pre_roll: std::collections::VecDeque<Vec<f32>>,
    phrase: Vec<f32>,
    in_speech: bool,
    loud_run: usize,
    quiet_run: usize,
    speech_frames: usize,
    frames_since_partial: usize,
    /// Offer the phrase-so-far for live transcription this often (frames).
    partial_every: usize,
    frames_seen: usize,
    /// Levels of the last `RECENT_FRAMES` frames of the current phrase.
    recent: std::collections::VecDeque<f32>,
    noise: f32,
    silence_frames: usize,
    min_speech_frames: usize,
    max_frames: usize,
    sensitivity: f32,
    /// Loudness of the most recent frame, for the meter.
    pub last_rms: f32,
}

impl Segmenter {
    pub fn new(cfg: &AudioConfig) -> Self {
        Self {
            pending: Vec::new(),
            pre_roll: Default::default(),
            phrase: Vec::new(),
            in_speech: false,
            loud_run: 0,
            quiet_run: 0,
            speech_frames: 0,
            frames_since_partial: 0,
            partial_every: (cfg.stream_interval_ms as usize / 30).max(10),
            frames_seen: 0,
            recent: Default::default(),
            noise: 0.005,
            silence_frames: (cfg.silence_ms as usize / 30).max(5),
            min_speech_frames: (cfg.min_speech_ms as usize / 30).max(1),
            max_frames: ((cfg.max_phrase_secs.max(2.0) * 1000.0) as usize) / 30,
            sensitivity: cfg.sensitivity.max(1.2),
            last_rms: 0.0,
        }
    }

    pub fn push(&mut self, samples: &[f32], out: &mut Vec<VadEvent>) {
        self.pending.extend_from_slice(samples);
        let mut offset = 0;
        while self.pending.len() - offset >= FRAME {
            let frame = self.pending[offset..offset + FRAME].to_vec();
            offset += FRAME;
            self.frame(frame, out);
        }
        self.pending.drain(..offset);
    }

    fn frame(&mut self, frame: Vec<f32>, out: &mut Vec<VadEvent>) {
        let level = rms(&frame);
        self.last_rms = level;
        self.frames_seen += 1;
        let warming_up = self.frames_seen <= WARMUP_FRAMES;
        let threshold = (self.noise * self.sensitivity).max(MIN_FLOOR);
        let loud = level > threshold;

        if !self.in_speech {
            // Track the background: fall quickly, rise slowly.
            let rate = match (warming_up, level < self.noise) {
                (true, _) => 0.3,
                (false, true) => 0.1,
                (false, false) => 0.01,
            };
            self.noise += (level - self.noise) * rate;

            self.pre_roll.push_back(frame);
            if self.pre_roll.len() > PRE_ROLL_FRAMES {
                self.pre_roll.pop_front();
            }
            let may_start = self.frames_seen > NO_START_FRAMES;
            self.loud_run = if loud && may_start {
                self.loud_run + 1
            } else {
                0
            };
            if self.loud_run >= START_FRAMES {
                self.in_speech = true;
                self.speech_frames = self.loud_run;
                self.quiet_run = 0;
                self.frames_since_partial = 0;
                self.phrase = self.pre_roll.drain(..).flatten().collect();
                log::debug!(
                    "speech started (level {level:.4}, background {:.4}, threshold {threshold:.4})",
                    self.noise
                );
                out.push(VadEvent::SpeechStarted);
            }
            return;
        }

        self.phrase.extend_from_slice(&frame);

        // Background noise can rise mid-phrase (a fan spinning up). Speech has
        // gaps between words, so if even the quietest recent moment is above
        // the floor, the floor is too low: move it up, or the phrase would
        // never end.
        self.recent.push_back(level);
        if self.recent.len() > RECENT_FRAMES {
            self.recent.pop_front();
            let quietest = self.recent.iter().copied().fold(f32::MAX, f32::min);
            if quietest > self.noise {
                self.noise += (quietest - self.noise) * 0.05;
            }
        }
        let threshold = (self.noise * self.sensitivity).max(MIN_FLOOR);
        let loud = level > threshold;

        if loud {
            self.speech_frames += 1;
            self.quiet_run = 0;
        } else {
            self.quiet_run += 1;
        }
        self.frames_since_partial += 1;

        let total_frames = self.phrase.len() / FRAME;
        if self.quiet_run >= self.silence_frames || total_frames >= self.max_frames {
            self.finish(out);
        } else if self.frames_since_partial >= self.partial_every {
            self.frames_since_partial = 0;
            out.push(VadEvent::Partial(self.phrase.clone()));
        }
    }

    /// Close the current phrase now (used when dictation is stopped mid-sentence).
    pub fn flush(&mut self, out: &mut Vec<VadEvent>) {
        if self.in_speech {
            self.finish(out);
        }
        self.pending.clear();
        self.pre_roll.clear();
    }

    fn finish(&mut self, out: &mut Vec<VadEvent>) {
        self.in_speech = false;
        self.recent.clear();
        self.loud_run = 0;
        let phrase = std::mem::take(&mut self.phrase);
        if self.speech_frames >= self.min_speech_frames {
            // Trim most of the trailing silence; Whisper sometimes invents
            // words ("Thank you.") to fill long quiet tails.
            let keep_tail = 8 * FRAME;
            let trim = (self.quiet_run * FRAME).saturating_sub(keep_tail);
            let end = phrase.len().saturating_sub(trim);
            log::debug!("phrase ended: {:.1}s", end as f32 / SAMPLE_RATE as f32);
            out.push(VadEvent::Phrase(phrase[..end].to_vec()));
        } else {
            log::debug!("ignored {} ms of noise", self.speech_frames * 30);
        }
        self.speech_frames = 0;
        self.quiet_run = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(secs: f32, amp: f32) -> Vec<f32> {
        (0..(secs * SAMPLE_RATE as f32) as usize)
            .map(|i| amp * (i as f32 * 0.07).sin())
            .collect()
    }

    #[test]
    fn detects_one_phrase_between_silences() {
        let mut seg = Segmenter::new(&AudioConfig::default());
        let mut ev = Vec::new();
        seg.push(&tone(1.0, 0.001), &mut ev);
        seg.push(&tone(1.5, 0.3), &mut ev);
        seg.push(&tone(1.5, 0.001), &mut ev);
        let phrases: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                VadEvent::Phrase(p) => Some(p.len()),
                _ => None,
            })
            .collect();
        assert_eq!(phrases.len(), 1);
        let secs = phrases[0] as f32 / SAMPLE_RATE as f32;
        assert!((1.5..2.3).contains(&secs), "phrase was {secs}s");
    }

    #[test]
    fn adapts_to_a_noisy_room() {
        // A steady fan well above the default floor must not count as speech.
        let mut seg = Segmenter::new(&AudioConfig::default());
        let mut ev = Vec::new();
        seg.push(&tone(3.0, 0.03), &mut ev);
        assert!(
            !ev.iter()
                .any(|e| matches!(e, VadEvent::Phrase(_) | VadEvent::SpeechStarted))
        );
        // ...while speech above it still does.
        seg.push(&tone(1.0, 0.4), &mut ev);
        seg.push(&tone(1.5, 0.03), &mut ev);
        assert!(ev.iter().any(|e| matches!(e, VadEvent::Phrase(_))));
    }

    #[test]
    fn phrases_still_end_when_the_background_gets_louder() {
        // Learn a silent room, then a fan starts under continuous talking
        // with short gaps; the phrase must end rather than run to the limit.
        let mut seg = Segmenter::new(&AudioConfig::default());
        let mut ev = Vec::new();
        seg.push(&tone(1.0, 0.0005), &mut ev);
        for _ in 0..6 {
            seg.push(&tone(0.6, 0.3), &mut ev);
            seg.push(&tone(0.15, 0.02), &mut ev);
        }
        seg.push(&tone(2.0, 0.02), &mut ev);
        let longest = ev
            .iter()
            .filter_map(|e| match e {
                VadEvent::Phrase(p) => Some(p.len() as f32 / SAMPLE_RATE as f32),
                _ => None,
            })
            .fold(0.0, f32::max);
        assert!(longest > 0.0 && longest < 8.0, "phrase lasted {longest}s");
    }

    #[test]
    fn ignores_short_clicks() {
        let mut seg = Segmenter::new(&AudioConfig::default());
        let mut ev = Vec::new();
        seg.push(&tone(1.0, 0.001), &mut ev);
        seg.push(&tone(0.12, 0.5), &mut ev);
        seg.push(&tone(1.5, 0.001), &mut ev);
        assert!(!ev.iter().any(|e| matches!(e, VadEvent::Phrase(_))));
    }
}

#[cfg(test)]
mod replay {
    use super::*;

    /// RV_VAD_WAV=file.wav cargo test replay -- --ignored --nocapture
    #[test]
    #[ignore]
    fn replay_wav() {
        let path = std::env::var("RV_VAD_WAV").expect("RV_VAD_WAV");
        let mut r = hound::WavReader::open(path).unwrap();
        let s: Vec<f32> = r
            .samples::<i16>()
            .map(|x| x.unwrap() as f32 / 32768.0)
            .collect();
        let mut seg = Segmenter::new(&AudioConfig::default());
        let mut ev = Vec::new();
        for (i, chunk) in s.chunks(480).enumerate() {
            seg.push(chunk, &mut ev);
            if i % 33 == 0 {
                eprintln!(
                    "{:5.1}s noise {:.4} in_speech {}",
                    i as f32 * 0.03,
                    seg.noise,
                    seg.in_speech
                );
            }
            for e in ev.drain(..) {
                match e {
                    VadEvent::SpeechStarted => eprintln!("{:5.1}s START", i as f32 * 0.03),
                    VadEvent::Phrase(p) => eprintln!(
                        "{:5.1}s PHRASE {:.1}s",
                        i as f32 * 0.03,
                        p.len() as f32 / 16000.0
                    ),
                    VadEvent::Partial(_) => {}
                }
            }
        }
    }
}
