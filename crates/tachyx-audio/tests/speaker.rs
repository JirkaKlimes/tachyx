//! The play path against this machine's own sound output.
//!
//! Ignored by default, because it makes a noise and because a build machine has
//! no speakers. Run it on a desktop with:
//!
//! ```text
//! cargo test -p tachyx-audio --test speaker -- --ignored --nocapture
//! ```
//!
//! Nothing here is a mock. The Opus is real Opus, encoded by libopus with the
//! settings the host uses, and it is pushed at the rate the host would send it,
//! so what the test proves is the whole path: that the device opens, that the
//! writes block rather than spin, that a hole is repaired or concealed, and that
//! a second of sound takes a second.

use std::time::{Duration, Instant};

use tachyx_audio::AudioPlayer;
use tachyx_client::AudioEvent;
use tachyx_proto::media::{FrameId, Timestamp};

/// Ten millisecond frames, which is what the host encodes.
const FRAME: usize = 480;

/// How many of them the test plays. A second.
const PACKETS: u32 = 100;

/// The packet the test throws away, to make a hole in the middle of the sound.
const EATEN: u32 = 40;

/// Quiet. This runs on somebody's desktop.
const GAIN: f32 = 0.05;

/// A second of a tone, in packets the host's encoder would have produced.
///
/// Forward error correction is on, as it is on the host, so the packet after the
/// hole carries a copy of what the hole swallowed and the play path has
/// something better than a guess to fill it with.
#[expect(clippy::cast_possible_truncation, reason = "a test fixture")]
fn tone() -> Vec<Vec<u8>> {
    let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
        .expect("libopus builds an encoder");
    encoder.set_inband_fec(true).expect("a settable encoder");
    encoder
        .set_packet_loss_perc(5)
        .expect("a settable encoder");
    let mut at = 0.0f32;
    (0..PACKETS)
        .map(|_| {
            let mut samples = Vec::with_capacity(FRAME * 2);
            for _ in 0..FRAME {
                let value = (8_000.0 * (at * 440.0 * std::f32::consts::TAU).sin()) as i16;
                samples.extend_from_slice(&[value, value]);
                at += 1.0 / 48_000.0;
            }
            encoder
                .encode_vec(&samples, 4_000)
                .expect("a frame of a tone encodes")
        })
        .collect()
}

#[test]
#[ignore = "makes a noise out of this machine's speakers"]
fn a_second_of_sound_takes_a_second_and_comes_out_whole() {
    let player = AudioPlayer::new().expect("this machine has a sound output");
    player.set_gain(GAIN);

    let packets = tone();
    let started = Instant::now();
    for (id, opus) in (0..PACKETS).zip(packets) {
        if id == EATEN {
            // The datagram the wire ate. The session would say so itself; here
            // it is said by hand, which is what the session's own tests cover.
            player.push(AudioEvent::Lost(1));
            continue;
        }
        player.push(AudioEvent::Packet {
            id: FrameId(id),
            capture: Timestamp::from_session_micros(u64::from(id) * 10_000),
            opus,
        });
        // The host sends one packet every ten milliseconds. Pushing them all at
        // once would prove nothing about the pacing and would be thrown away by
        // the buffer's own ceiling.
        std::thread::sleep(Duration::from_millis(10));
    }
    // The buffer holds a little sound back, so the last of it is still playing.
    std::thread::sleep(Duration::from_millis(100));

    let stats = player.stats();
    let elapsed = started.elapsed();
    println!("{stats:?} in {elapsed:?}");
    assert_eq!(player.failure(), None, "the device took every frame");
    assert!(
        stats.played >= u64::from(PACKETS) - 10,
        "nearly every packet has to be heard: {stats:?}"
    );
    assert_eq!(
        stats.recovered, 1,
        "the hole is filled out of the copy the packet behind it carries, which \
         is what the encoder's forward error correction is for: {stats:?}"
    );
    assert!(
        stats.buffered <= 10,
        "the buffer is a jitter buffer and not a queue: {stats:?}"
    );
    assert!(
        elapsed > Duration::from_millis(900) && elapsed < Duration::from_millis(2_000),
        "a second of sound takes a second, and the device is what says so: \
         {elapsed:?}"
    );
}
