//! What a decoder is told beyond the bitstream itself.

use tachyx_proto::control::{Codec, Codecs};

/// The codecs this client can read on Android.
///
/// The ceiling rather than the advertisement: everything here has a reader in
/// [`super::bitstream`], which is what configures `MediaCodec` from the stream
/// alone so that a decoder survives a reconnection and a resolution change with
/// nothing said to it. What the client actually advertises is this set cut down
/// to what the device decodes in hardware, and that is probed where the
/// platform is in reach; see [`super::present::decoders`].
pub(super) const DECODES: Codecs = Codecs::of(Codec::H264).with(Codec::Av1);

/// A codec's name as `MediaCodec` spells it.
pub(super) fn mime(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "video/avc",
        Codec::Av1 => "video/av01",
    }
}

/// What the decoder is asked for in the way of latency, from everything down to
/// nothing.
///
/// Every key is an integer set to one. The first is Android's own, from API 30.
/// The rest are the vendor keys that stand in for it on devices older than that,
/// and they are the reason this is a ladder rather than a list: a decoder that
/// does not know a vendor key fails to configure rather than ignoring it, so
/// each rung is offered until one is taken.
///
/// The keys deliberately absent matter as much. The operating rate crashes
/// several shipping devices outright. The output reorder depth and the maximum
/// B frame count are encoder settings and do nothing to a decoder; what they
/// would have asked for is already true, because the host's stream declares
/// that it reorders nothing.
pub(super) const LOW_LATENCY: [&[&str]; 3] = [
    &[
        "low-latency",
        "vendor.qti-ext-dec-low-latency.enable",
        "vendor.qti-ext-dec-picture-order.enable",
        "vendor.low-latency.enable",
    ],
    &["low-latency"],
    &[],
];

#[cfg(test)]
mod tests {
    use tachyx_proto::control::Codec;

    use super::{DECODES, LOW_LATENCY, mime};

    #[test]
    fn every_rung_asks_for_less_than_the_one_above_it() {
        for pair in LOW_LATENCY.windows(2) {
            let (upper, lower) = (pair[0], pair[1]);
            assert!(
                lower.len() < upper.len(),
                "{lower:?} is no smaller than {upper:?}"
            );
            for key in lower {
                assert!(
                    upper.contains(key),
                    "{key} survives below the rung that drops it"
                );
            }
        }
    }

    #[test]
    fn the_last_rung_asks_for_nothing_a_decoder_can_refuse() {
        assert_eq!(
            LOW_LATENCY.last(),
            Some(&(&[] as &[&str])),
            "a device that rejects every latency key must still get a decoder"
        );
    }

    #[test]
    fn the_first_rung_asks_for_the_key_android_itself_defines() {
        assert!(LOW_LATENCY[0].contains(&"low-latency"));
    }

    #[test]
    fn nothing_asks_for_a_setting_that_is_not_a_decoder_s() {
        for rung in LOW_LATENCY {
            for key in rung {
                assert!(
                    !["operating-rate", "output-reorder-depth", "max-bframes"].contains(key),
                    "{key} either belongs to an encoder or crashes devices"
                );
            }
        }
    }

    #[test]
    fn every_codec_with_a_reader_has_a_name_mediacodec_knows() {
        assert!(DECODES.contains(Codec::H264));
        assert!(DECODES.contains(Codec::Av1));
        assert_eq!(mime(Codec::H264), "video/avc");
        assert_eq!(mime(Codec::Av1), "video/av01");
    }
}
