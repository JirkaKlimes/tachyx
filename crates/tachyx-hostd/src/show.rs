//! Turning what the daemon said into what goes on the terminal.
//!
//! Two audiences, and they are not the same. Text is for a person reading a
//! terminal: aligned, in words, and it may change whenever it reads better.
//! `--json` is for a script: one JSON value, field names that do not move, and
//! nothing else on standard output.
//!
//! Every function here returns the text rather than printing it, which is what
//! makes the promise above testable.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use tachyx_crypto::{DeviceId, RelayAddress};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::control::{Pairing, Report, State, Status};
use crate::qr;

/// Which audience is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A person at a terminal.
    Human,
    /// A script.
    Json,
}

impl Format {
    /// What `--json` asked for.
    #[must_use]
    pub const fn of(json: bool) -> Self {
        if json {
            Self::Json
        } else {
            Self::Human
        }
    }
}

/// The width of the label column in the text output.
const LABEL: usize = 10;

/// The width of the name column in the list of paired devices.
const NAME: usize = 16;

/// Text that came from a peer or out of a file, made safe to put on a terminal.
///
/// A device chooses the name this host prints, and a relay address is whatever
/// somebody wrote in the file. Neither is this build's to trust, and a terminal
/// reads some of what it is given as instructions rather than as text.
///
/// [`crate::pairing::label`] does this to a name at the moment it is written
/// down. This does it again at the moment it is read back, because a file
/// written by an older build, or edited by hand, never went through that.
///
/// Out go the control characters, which is where an escape sequence starts. Out
/// go the format characters: a bidi override reverses the rest of the row it
/// lands in, and the zero-width ones hide text inside other text. Out go the
/// private use characters, which no two fonts draw the same, and the line and
/// paragraph separators, which end a row this build was in the middle of
/// writing. Surrogates are not in the list because a `char` cannot hold one.
///
/// A combining mark with nothing in front of it goes as well. It would attach
/// to whatever the terminal drew last, which is this build's own column
/// separator.
#[must_use]
pub fn printable(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    for character in text.trim().chars() {
        if character.is_control() || invisible(character) {
            continue;
        }
        if character.width().unwrap_or(0) == 0 && kept.is_empty() {
            continue;
        }
        kept.push(character);
    }
    kept.truncate(kept.trim_end().len());
    kept
}

/// [`printable`], and no wider than `columns` on the terminal.
///
/// Counting characters is the wrong measure: sixty-four CJK characters are a
/// hundred and twenty-eight columns, and a combining mark is none at all. A
/// character is kept whole or dropped whole, because half of a double-width one
/// is not a thing a terminal can draw.
#[must_use]
pub fn fit(text: &str, columns: usize) -> String {
    let mut kept = printable(text);
    let mut used = 0;
    let over = kept.char_indices().find_map(|(at, character)| {
        used += character.width().unwrap_or(0);
        (used > columns).then_some(at)
    });
    if let Some(over) = over {
        kept.truncate(over);
    }
    kept.truncate(kept.trim_end().len());
    kept
}

/// One value in a column `columns` wide, padded by what it will occupy.
///
/// `{:<16}` pads by `chars().count()`, so a name of sixteen CJK characters is
/// given no padding at all and takes thirty-two columns, and everything after
/// it on the row sits somewhere else.
fn column(text: &str, columns: usize) -> String {
    let text = printable(text);
    let padding = columns.saturating_sub(text.width());
    format!("{text}{blank:padding$}", blank = "")
}

/// Whether `character` is one a terminal is not going to draw: `Cf`, `Co`, `Zl`
/// or `Zp`.
///
/// The ranges are Unicode 15.1's. A category that gains a member is a character
/// that gets through, and the ones that reorder text or hide it are all here
/// already.
const fn invisible(character: char) -> bool {
    matches!(character,
        '\u{00ad}'
        | '\u{0600}'..='\u{0605}'
        | '\u{061c}'
        | '\u{06dd}'
        | '\u{070f}'
        | '\u{0890}'..='\u{0891}'
        | '\u{08e2}'
        | '\u{180e}'
        | '\u{2028}'..='\u{2029}'
        | '\u{200b}'..='\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206f}'
        | '\u{e000}'..='\u{f8ff}'
        | '\u{feff}'
        | '\u{fff9}'..='\u{fffb}'
        | '\u{110bd}'
        | '\u{110cd}'
        | '\u{13430}'..='\u{1343f}'
        | '\u{1bca0}'..='\u{1bca3}'
        | '\u{1d173}'..='\u{1d17a}'
        | '\u{e0001}'
        | '\u{e0020}'..='\u{e007f}'
        | '\u{f0000}'..='\u{ffffd}'
        | '\u{100000}'..='\u{10fffd}')
}

/// This host's identifier.
#[must_use]
pub fn identifier(device: DeviceId, format: Format) -> String {
    match format {
        Format::Human => format!("{}\n", device.to_base32()),
        Format::Json => line(&json!({ "device": device.to_base32() })),
    }
}

/// A change that happened and has nothing to report.
///
/// Text says nothing, the way every other command that did what it was asked
/// says nothing. `--json` still has to be one value, so it is this one.
#[must_use]
pub fn done(format: Format) -> String {
    match format {
        Format::Human => String::new(),
        Format::Json => line(&json!({ "ok": true })),
    }
}

/// What the daemon is doing.
#[must_use]
pub fn status(status: &Status, format: Format) -> String {
    if format == Format::Json {
        return line(status);
    }
    let mut text = String::new();
    row(&mut text, "device", &status.device.to_base32());
    row(&mut text, "name", &printable(&status.name));
    row(&mut text, "version", &status.version);
    row(&mut text, "uptime", &span(status.uptime));
    row(&mut text, "listen", &status.listen);
    row(
        &mut text,
        "encoders",
        &if status.encoders.is_empty() {
            "none".to_owned()
        } else {
            status.encoders.join(", ")
        },
    );
    row(&mut text, "paired", &format!("{} devices", status.paired));
    if status.relays.is_empty() {
        row(&mut text, "relays", "none configured");
    } else {
        for (index, relay) in status.relays.iter().enumerate() {
            let state = match (relay.connected, relay.rtt) {
                (true, Some(rtt)) => format!("connected, {rtt} ms away"),
                (true, None) => "connected".to_owned(),
                (false, _) => "not connected".to_owned(),
            };
            row(
                &mut text,
                if index == 0 { "relays" } else { "" },
                &format!("{} {state}", printable(&relay.relay.address)),
            );
        }
    }
    if status.sessions.is_empty() {
        row(&mut text, "sessions", "none");
        return text;
    }
    row(&mut text, "sessions", "");
    for session in &status.sessions {
        let _ = writeln!(
            text,
            "  {}  {} ({}) at {}, {}",
            session.id,
            printable(&session.name),
            session.device,
            printable(&session.address),
            if session.relayed { "relayed" } else { "direct" }
        );
        let _ = writeln!(
            text,
            "     {} {}x{} {}, {}, host {:.1} ms, rtt {} ms, loss {:.1}%, sent {}, \
             received {}, up {}",
            session.codec,
            session.width,
            session.height,
            frames(session.millihertz),
            rate(session.bitrate),
            f64::from(session.pipeline_micros) / 1000.0,
            session.rtt,
            session.loss * 100.0,
            size(session.sent),
            size(session.received),
            span(session.uptime),
        );
    }
    text
}

/// The devices this host has paired with.
#[must_use]
pub fn pairings(pairings: &[Pairing], format: Format) -> String {
    if format == Format::Json {
        return line(&json!({ "pairings": pairings }));
    }
    if pairings.is_empty() {
        return "no device is paired, so this host accepts nobody. run `tachyx-hostd pair`\n"
            .to_owned();
    }
    let mut text = String::new();
    for pairing in pairings {
        let _ = writeln!(
            text,
            "{}  {}  {:<8} {:<11} paired {} ago",
            pairing.device,
            column(&pairing.name, NAME),
            if pairing.control { "control" } else { "view" },
            if pairing.unattended {
                "unattended"
            } else {
                "attended"
            },
            span(age(pairing.paired)),
        );
    }
    text
}

/// The relays this host looks for.
#[must_use]
pub fn relays(relays: &[RelayAddress], format: Format) -> String {
    if format == Format::Json {
        return line(&json!({
            "relays": relays.iter().map(ToString::to_string).collect::<Vec<_>>(),
        }));
    }
    if relays.is_empty() {
        return "no relay is configured. run `tachyx-hostd relay add <address:port#KEY>`\n"
            .to_owned();
    }
    let mut text = String::new();
    for relay in relays {
        let _ = writeln!(text, "{}", printable(&relay.to_string()));
    }
    text
}

/// What `doctor` found.
#[must_use]
pub fn report(report: &Report, format: Format) -> String {
    if format == Format::Json {
        return line(&json!({ "checks": report.checks, "passed": report.passed() }));
    }
    let mut text = String::new();
    for check in &report.checks {
        let mark = match check.state {
            State::Pass => "ok  ",
            State::Warn => "warn",
            State::Fail => "FAIL",
        };
        // A detail names paths and relay addresses that came out of a file.
        let _ = writeln!(text, "{mark}  {}", printable(&check.detail));
    }
    text
}

/// The code, as soon as there is one.
///
/// The QR is drawn beside the digits rather than instead of them: a camera is
/// the fast path and eight digits is the one that always works.
#[must_use]
pub fn waiting(typed: &str, uri: &str, ttl: u64, drawn: Option<&str>, format: Format) -> String {
    if format == Format::Json {
        return line(&json!({ "event": "waiting", "typed": typed, "uri": uri }));
    }
    let mut text = format!("\n{typed}\n\n");
    if let Some(drawn) = drawn {
        let _ = writeln!(text, "{drawn}");
    }
    let _ = writeln!(
        text,
        "scan the code or type it into the app. it is good for {}.",
        span(ttl)
    );
    text
}

/// Somebody answered.
#[must_use]
pub fn joined(format: Format) -> String {
    match format {
        Format::Human => "somebody answered. checking the code\n".to_owned(),
        Format::Json => line(&json!({ "event": "joined" })),
    }
}

/// A device is paired.
#[must_use]
pub fn succeeded(device: DeviceId, name: &str, words: &str, format: Format) -> String {
    match format {
        Format::Human => format!(
            "\npaired: {} ({device})\nboth screens should show: {words}\n",
            printable(name)
        ),
        Format::Json => line(&json!({
            "event": "succeeded",
            "device": device.to_base32(),
            "name": name,
            "words": words,
        })),
    }
}

/// What went wrong, for standard error.
///
/// A reason is quite often a peer's or a file's own words: the daemon's refusal
/// arrives as one, and a selector that named two devices names them both back.
#[must_use]
pub fn failure(reason: &str, format: Format) -> String {
    match format {
        Format::Human => format!("{}\n", printable(reason)),
        Format::Json => line(&json!({ "error": reason })),
    }
}

/// Draws the pairing link, or says why it could not be drawn.
///
/// A link that will not encode is not a reason to withhold the digits, so this
/// answers with what it can rather than failing the command.
#[must_use]
pub fn code(uri: &str) -> String {
    match qr::draw(uri, qr::Terminal::probe()) {
        Ok(drawn) => drawn,
        Err(error) => format!("the link will not draw as a qr code ({error}):\n{uri}\n"),
    }
}

/// One `label  value` row, or a continuation row when the label is empty.
fn row(text: &mut String, label: &str, value: &str) {
    let _ = writeln!(text, "{label:<LABEL$}{value}");
}

/// One JSON value and a newline, which is the whole of `--json` for one
/// command.
fn line<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|error| json!({ "error": error.to_string() }).to_string())
        + "\n"
}

/// A number of seconds, in the largest two units that say anything.
fn span(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3600),
    }
}

/// How long ago something at `at` seconds since the Unix epoch was.
///
/// A time in the future is a clock that moved, and it reads as now rather than
/// as a negative age.
fn age(at: u64) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
        .saturating_sub(at)
}

/// A frame rate, from the thousandths of a hertz the encoder is told.
fn frames(millihertz: u32) -> String {
    if millihertz.is_multiple_of(1000) {
        format!("{} fps", millihertz / 1000)
    } else {
        format!("{:.1} fps", f64::from(millihertz) / 1000.0)
    }
}

/// A bitrate, in the unit a person would say it in.
fn rate(bits: u32) -> String {
    match bits {
        0..1_000 => format!("{bits} bit/s"),
        1_000..1_000_000 => format!("{} kbit/s", bits / 1_000),
        _ => format!("{:.1} Mbit/s", f64::from(bits) / 1_000_000.0),
    }
}

/// A count of bytes, in the unit a person would say it in.
#[expect(
    clippy::cast_precision_loss,
    reason = "a byte count is being rounded to one decimal for a person to read"
)]
fn size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{} kB", bytes / 1_000),
        1_000_000..1_000_000_000 => format!("{:.1} MB", bytes as f64 / 1e6),
        _ => format!("{:.1} GB", bytes as f64 / 1e9),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Check, RelayStatus, SessionStatus};
    use crate::sessions::SessionId;

    fn device(seed: u8) -> DeviceId {
        tachyx_crypto::DeviceKey::from_seed(&[seed; 32])
            .expect("a key")
            .id()
    }

    fn a_status() -> Status {
        Status {
            device: device(1),
            name: "desk".to_owned(),
            version: "0.1.0".to_owned(),
            uptime: 11_530,
            listen: "0.0.0.0:47989".to_owned(),
            encoders: vec!["h264".to_owned(), "av1".to_owned()],
            paired: 2,
            relays: vec![RelayStatus {
                relay: RelayAddress {
                    address: "100.64.0.2:47990".to_owned(),
                    key: Some(device(2)),
                },
                connected: true,
                rtt: Some(12),
            }],
            sessions: vec![SessionStatus {
                id: SessionId(1),
                device: device(3),
                name: "phone".to_owned(),
                address: "10.0.0.5:41234".to_owned(),
                relayed: false,
                codec: "h264".to_owned(),
                width: 1920,
                height: 1080,
                millihertz: 60_000,
                bitrate: 8_200_000,
                rtt: 12,
                pipeline_micros: 9_300,
                loss: 0.001,
                uptime: 754,
                sent: 240_000_000,
                received: 3_100_000,
            }],
        }
    }

    #[test]
    fn what_json_prints_reads_back_as_one_json_value_with_the_fields_it_promised() {
        let printed = status(&a_status(), Format::Json);
        assert!(printed.ends_with('\n'));
        let value: serde_json::Value =
            serde_json::from_str(&printed).expect("one json value and nothing else");
        assert_eq!(value["device"], device(1).to_base32());
        assert_eq!(value["name"], "desk");
        assert_eq!(value["paired"], 2);
        assert_eq!(value["relays"][0]["connected"], true);
        assert_eq!(value["relays"][0]["rtt"], 12);
        assert_eq!(value["sessions"][0]["id"], 1);
        assert_eq!(value["sessions"][0]["codec"], "h264");
        assert_eq!(value["sessions"][0]["relayed"], false);
    }

    #[test]
    fn every_json_answer_is_one_value_a_parser_takes() {
        let answers = [
            identifier(device(1), Format::Json),
            done(Format::Json),
            status(&a_status(), Format::Json),
            pairings(
                &[Pairing {
                    device: device(4),
                    name: "phone".to_owned(),
                    control: true,
                    unattended: true,
                    paired: 1_700_000_000,
                }],
                Format::Json,
            ),
            relays(
                &[RelayAddress {
                    address: "100.64.0.2:47990".to_owned(),
                    key: Some(device(2)),
                }],
                Format::Json,
            ),
            report(
                &Report {
                    checks: vec![Check::new("encoder", true, "this host encodes h264")],
                },
                Format::Json,
            ),
            waiting("7-4921-8306", "tachyx://pair?c=AA", 300, None, Format::Json),
            joined(Format::Json),
            succeeded(device(5), "phone", "🐜 🌵 ⭐", Format::Json),
            failure("the daemon is not running", Format::Json),
        ];
        for answer in answers {
            assert_eq!(answer.lines().count(), 1, "{answer}");
            serde_json::from_str::<serde_json::Value>(&answer)
                .unwrap_or_else(|error| panic!("{answer} is not json: {error}"));
        }
    }

    #[test]
    fn a_json_pairing_row_names_its_fields_the_way_a_script_will_read_them() {
        let printed = pairings(
            &[Pairing {
                device: device(4),
                name: "phone".to_owned(),
                control: false,
                unattended: true,
                paired: 1_700_000_000,
            }],
            Format::Json,
        );
        let value: serde_json::Value = serde_json::from_str(&printed).expect("json");
        let row = &value["pairings"][0];
        assert_eq!(row["device"], device(4).to_base32());
        assert_eq!(row["name"], "phone");
        assert_eq!(row["control"], false);
        assert_eq!(row["unattended"], true);
        assert_eq!(row["paired"], 1_700_000_000_u64);
    }

    #[test]
    fn a_doctor_report_says_which_check_failed_in_both_forms() {
        let report_of = Report {
            checks: vec![
                Check::new("daemon", true, "the daemon is running"),
                Check::new("encoder", false, "no video encoder"),
                Check::warn("pairings", "no device is paired"),
            ],
        };
        let text = report(&report_of, Format::Human);
        assert!(text.contains("ok    the daemon is running"), "{text}");
        assert!(text.contains("FAIL  no video encoder"), "{text}");
        assert!(text.contains("warn  no device is paired"), "{text}");

        let value: serde_json::Value =
            serde_json::from_str(&report(&report_of, Format::Json)).expect("json");
        assert_eq!(value["passed"], false);
        assert_eq!(value["checks"][1]["check"], "encoder");
        assert_eq!(value["checks"][1]["state"], "fail");
        assert_eq!(value["checks"][2]["state"], "warn");
    }

    #[test]
    fn the_text_status_says_everything_a_person_asked_for() {
        let text = status(&a_status(), Format::Human);
        assert!(text.contains("uptime    3h 12m"), "{text}");
        assert!(
            text.contains("100.64.0.2:47990 connected, 12 ms away"),
            "{text}"
        );
        assert!(text.contains("h264 1920x1080 60 fps, 8.2 Mbit/s"), "{text}");
        assert!(
            text.contains("host 9.3 ms, rtt 12 ms, loss 0.1%"),
            "the host's own half of the latency is what a client cannot \
             measure for itself: {text}"
        );
        assert!(
            text.contains("sent 240.0 MB, received 3.1 MB, up 12m"),
            "{text}"
        );
    }

    #[test]
    fn a_name_out_of_a_file_is_filtered_when_it_is_read_back_and_not_only_when_it_is_written() {
        // `pairing::label` never ran over this: the file was written by an older
        // build, or edited by hand.
        let hostile = Pairing {
            device: device(4),
            name: "\u{1b}[41m\u{1b}[2JPWNED\u{202e}phone".to_owned(),
            control: true,
            unattended: true,
            paired: 1_700_000_000,
        };
        let printed = pairings(std::slice::from_ref(&hostile), Format::Human);
        assert!(!printed.contains('\u{1b}'), "{printed:?}");
        assert!(!printed.contains('\u{202e}'), "{printed:?}");
        assert_eq!(printed.lines().count(), 1, "{printed:?}");
    }

    #[test]
    fn a_relay_address_out_of_a_file_is_filtered_the_same_way() {
        let planted = RelayAddress {
            address: "\u{1b}[41m\u{1b}[2JPWNED\naaa:1".to_owned(),
            key: Some(device(2)),
        };
        let printed = relays(std::slice::from_ref(&planted), Format::Human);
        assert!(!printed.contains('\u{1b}'), "{printed:?}");
        assert_eq!(printed.lines().count(), 1, "{printed:?}");

        let refused = failure("cannot add \u{1b}[2Jthat", Format::Human);
        assert!(!refused.contains('\u{1b}'), "{refused:?}");
    }

    #[test]
    fn a_column_is_padded_by_the_columns_it_takes_and_not_the_characters_it_has() {
        assert_eq!(column("phone", 8).width(), 8);
        assert_eq!(
            column("\u{4e2d}\u{6587}", 8).width(),
            8,
            "two CJK characters are four columns, so this is four spaces and not six"
        );
        assert_eq!(
            column("e\u{301}", 4).width(),
            4,
            "a mark is no columns at all"
        );
        assert_eq!(
            column(&"a".repeat(12), 8),
            "a".repeat(12),
            "a name wider than its column is not cut, the way a long one never was"
        );
    }

    #[test]
    fn a_span_is_the_two_units_that_say_something() {
        assert_eq!(span(0), "0s");
        assert_eq!(span(59), "59s");
        assert_eq!(span(60), "1m");
        assert_eq!(span(3_599), "59m");
        assert_eq!(span(3_600), "1h 0m");
        assert_eq!(span(86_399), "23h 59m");
        assert_eq!(span(86_400), "1d 0h");
        assert_eq!(span(200_000), "2d 7h");
    }

    #[test]
    fn a_clock_that_moved_backwards_does_not_make_a_pairing_from_the_future() {
        let ahead = age(u64::MAX);
        assert_eq!(ahead, 0);
    }
}
