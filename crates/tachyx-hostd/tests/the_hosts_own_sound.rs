//! The sound path against the machine this runs on.
//!
//! Ignored by default, because it needs a sound server, the GStreamer plugins
//! and an output device, and a machine with none of those is not a machine with
//! a fault. Run it with `cargo test -p tachyx-hostd --test the_hosts_own_sound
//! -- --ignored --nocapture`.
//!
//! Nothing here is a mock. The point of the file is that every claim the sound
//! path makes about devices, timestamps, volume and the tap being before the
//! mute is made against a real server rather than against a reading of its
//! documentation.
//!
//! It plays a tone into this machine's own output, and it mutes that output
//! first. That is not politeness: the tap this records from is before the
//! sink's own mute, so a tone recorded while the room hears nothing is the
//! whole of what `AudioMode::Exclusive` promises, demonstrated. The mute and
//! the volume are both put back exactly as they were found.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tachyx_hostd::audio::{self, HostAudio, Sink};
use tachyx_media::{AUDIO_FRAME, AudioCapture, Clock, OpusPackets};
use tachyx_proto::control::VOLUME_NORM;
use tachyx_proto::media::Timestamp;

/// How long each half of the recording runs for.
const RUN: Duration = Duration::from_secs(2);

/// How long a change made through the sound server has to come back.
const SETTLE: Duration = Duration::from_secs(2);

#[test]
#[ignore = "needs a sound server, an output device and the gstreamer plugins"]
fn the_hosts_own_output_comes_out_as_opus_on_the_sessions_clock() {
    // The daemon's own probe, which is what decides whether a hello announces
    // sound at all.
    let probed = audio::probe().expect("a sound server with a default sink");
    let audio = HostAudio::open().expect("a sound server with a default sink");
    let sink = audio.sink();
    assert_eq!(probed.name, sink.name, "one machine, one default sink");
    // Held for the whole recording. Everything below is captured and none of it
    // is heard.
    let silence = audio.silence();
    assert!(
        settled(&audio, |sink| sink.muted),
        "the sound server did not take the mute, so the tone below would be audible"
    );

    // The clock a session stamps its pictures against, made before capture
    // starts exactly as a session makes it.
    let clock = Clock::now();
    let (capture, packets) =
        AudioCapture::start(&sink.monitor(), clock).expect("the sound pipeline starts");

    let quiet = collect(&packets, RUN);
    let mut tone = play_a_tone();
    let loud = collect(&packets, RUN);
    let _ = tone.kill();
    let _ = tone.wait();

    let stopping = Instant::now();
    capture.stop().expect("the pipeline stops cleanly");
    let stopped = stopping.elapsed();
    drop(silence);

    let first = quiet.first().expect("sound arrived").captured;
    let last = quiet.last().expect("sound arrived").captured;
    let gaps = u64::try_from(quiet.len() - 1).expect("a small count");
    let gap = last.micros_since(first) / gaps;
    let started = first.micros_since(Timestamp::from_session_micros(0));
    println!(
        "\n  device             {}\n  \
           without a tone     {} packets, {} bytes mean, {} largest\n  \
           with a 440 Hz tone {} packets, {} bytes mean, {} largest\n  \
           first stamped at   {started} microseconds into the session\n  \
           packets are        {gap} microseconds apart, against {AUDIO_FRAME:?}\n  \
           stopped in         {stopped:?}\n",
        sink.monitor(),
        quiet.len(),
        mean(&quiet),
        largest(&quiet),
        loud.len(),
        mean(&loud),
        largest(&loud),
    );

    // A hundred packets a second, less whatever the pipeline spent starting.
    let expected = usize::try_from(RUN.as_millis() / AUDIO_FRAME.as_millis()).expect("a short run");
    for held in [&quiet, &loud] {
        assert!(
            (expected - expected / 4..=expected + 2).contains(&held.len()),
            "{} packets where a 10 ms frame over {RUN:?} is about {expected}",
            held.len()
        );
        assert!(
            held.iter().all(|packet| !packet.bytes.is_empty()),
            "an empty packet is not something a datagram can carry"
        );
        assert!(
            held.windows(2)
                .all(|pair| pair[1].captured.micros_since(pair[0].captured) > 0),
            "timestamps must move forward"
        );
    }

    // The tap is before the sink's own mute. Nothing was audible for the whole
    // of this test, and the encoder still had a tone to code.
    //
    // Measured against what a real signal costs rather than against the quiet
    // half, because the quiet half is only quiet if nobody is using the machine:
    // a run of this with a browser playing something read 160 bytes a packet
    // with no tone at all. Digital silence codes in three bytes and a 10 ms
    // frame of a signal at 128 kbit/s stereo in about a hundred and sixty, so
    // sixty separates them with room on both sides.
    assert!(
        mean(&loud) > 60,
        "a tone played into a muted sink must still reach the monitor, and cost {} bytes a \
         packet against {} for the half with no tone",
        mean(&loud),
        mean(&quiet)
    );

    // One frame of sound per packet. Not exactly, because the figure compares
    // the sound card's own clock against the system clock and no two crystals
    // agree; a percent is far inside anything a jitter buffer notices and far
    // outside the two clocks being different clocks.
    let frame = u64::try_from(AUDIO_FRAME.as_micros()).expect("a short frame");
    assert!(
        frame.abs_diff(gap) * 100 < frame,
        "{gaps} packets are {gap} microseconds apart, against a frame of {frame}"
    );

    // The clock domain, and the only check that catches it. GStreamer's own
    // clock is `CLOCK_MONOTONIC` and so is the session's, so the first packet is
    // stamped a fraction of a second after the clock was made. A pipeline left
    // to take the sound server's clock instead would stamp it thousands of
    // seconds out, and sound and picture would never be laid together.
    assert!(
        started < 1_000_000,
        "the first packet is stamped {started} microseconds into a session that had just \
         started, so sound and picture are not on one clock"
    );
    assert!(
        stopped < Duration::from_secs(1),
        "stopping took {stopped:?}, so a mode change would stall the session"
    );

    assert!(
        settled(&audio, |now| now.muted == sink.muted),
        "dropping the guard must put this machine's own mute back"
    );
}

#[test]
#[ignore = "needs a sound server and moves this machine's volume, briefly"]
fn the_hosts_volume_and_mute_are_read_set_and_restored() {
    let audio = HostAudio::open().expect("a sound server with a default sink");
    let opened = audio.sink();
    assert!(opened.volume <= VOLUME_NORM);
    assert!(!opened.name.is_empty());

    // A step this machine's owner will not notice, in whichever direction there
    // is room for one, and put back exactly at the end.
    let moved = if opened.volume >= 50 {
        opened.volume - 50
    } else {
        opened.volume + 50
    };
    audio.set_volume(moved);
    assert!(
        settled(&audio, |sink| sink.volume == moved),
        "the sound server did not take a volume of {moved} permille"
    );

    audio.set_muted(!opened.muted);
    assert!(
        settled(&audio, |sink| sink.muted != opened.muted),
        "the sound server did not take the mute"
    );

    audio.set_volume(opened.volume);
    audio.set_muted(opened.muted);
    let restored = settled(&audio, |sink| *sink == opened);
    println!(
        "\n  sink               {}\n  \
           found at           {} permille, muted {}\n  \
           moved to           {moved} permille and muted {}\n  \
           left at            {} permille, muted {}\n",
        opened.name,
        opened.volume,
        opened.muted,
        !opened.muted,
        audio.sink().volume,
        audio.sink().muted,
    );
    assert!(
        restored,
        "this test must leave the machine exactly as it found it"
    );
}

/// Takes packets for `run`, and stops when the pipeline does.
fn collect(packets: &OpusPackets, run: Duration) -> Vec<tachyx_media::OpusPacket> {
    let deadline = Instant::now() + run;
    let mut held = Vec::new();
    while Instant::now() < deadline {
        let Ok(packet) = packets.recv_timeout(Duration::from_millis(500)) else {
            break;
        };
        held.push(packet);
    }
    held
}

/// Plays a quiet sine wave into this machine's default output.
///
/// Through `gst-launch-1.0` rather than a pipeline of our own, so that the test
/// pushes sound in by a path that shares nothing with the one it is measuring.
fn play_a_tone() -> Child {
    Command::new("gst-launch-1.0")
        .args([
            "audiotestsrc",
            "wave=sine",
            "freq=440",
            "volume=0.2",
            "!",
            "audioconvert",
            "!",
            "pulsesink",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("gst-launch-1.0 is in the development shell")
}

/// Waits for the host's own output to satisfy `wanted`.
///
/// Polled rather than awaited, because these tests hold no runtime. What is
/// being waited for is a round trip to the sound server, which takes under a
/// millisecond and never [`SETTLE`].
fn settled(audio: &HostAudio, wanted: impl Fn(&Sink) -> bool) -> bool {
    let deadline = Instant::now() + SETTLE;
    while Instant::now() < deadline {
        if wanted(&audio.sink()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn mean(packets: &[tachyx_media::OpusPacket]) -> usize {
    packets
        .iter()
        .map(|packet| packet.bytes.len())
        .sum::<usize>()
        / packets.len().max(1)
}

fn largest(packets: &[tachyx_media::OpusPacket]) -> usize {
    packets
        .iter()
        .map(|packet| packet.bytes.len())
        .max()
        .unwrap_or(0)
}
