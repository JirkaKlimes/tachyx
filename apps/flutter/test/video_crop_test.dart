import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:tachyx/src/rust/api/session.dart' show Encoding;
import 'package:tachyx/src/ui/video_texture_view.dart';

Encoding _encoding({
  required int codedWidth,
  required int codedHeight,
  required int visibleWidth,
  required int visibleHeight,
}) => Encoding(
  codedWidth: codedWidth,
  codedHeight: codedHeight,
  visibleWidth: visibleWidth,
  visibleHeight: visibleHeight,
  aspectWidth: 16,
  aspectHeight: 9,
  framerate: 60,
  bitrate: 20000000,
);

void main() {
  test('a picture that fills its macroblocks is not cropped at all', () {
    // 1280x720 and 3440x1440 need no crop: both are multiples of sixteen.
    final VideoCrop crop = VideoCrop.of(
      _encoding(
        codedWidth: 3440,
        codedHeight: 1440,
        visibleWidth: 3440,
        visibleHeight: 1440,
      ),
    );
    expect(crop, VideoCrop.whole);
    expect(crop.isWhole, isTrue);
  });

  test('eight rows the encoder had to add are scaled back off', () {
    // What the host sends for a 1080 row picture.
    final VideoCrop crop = VideoCrop.of(
      _encoding(
        codedWidth: 1920,
        codedHeight: 1088,
        visibleWidth: 1920,
        visibleHeight: 1080,
      ),
    );
    expect(crop.scaleX, 1);
    expect(crop.scaleY, closeTo(1088 / 1080, 1e-12));
    expect(crop.isWhole, isFalse);
  });

  test('a picture cropped in both directions is scaled in both', () {
    // 2580x1080 visible, which this host really does encode as 2592x1088.
    final VideoCrop crop = VideoCrop.of(
      _encoding(
        codedWidth: 2592,
        codedHeight: 1088,
        visibleWidth: 2580,
        visibleHeight: 1080,
      ),
    );
    expect(crop.scaleX, closeTo(2592 / 2580, 1e-12));
    expect(crop.scaleY, closeTo(1088 / 1080, 1e-12));
  });

  test('the scale never shrinks the picture, whatever the host says', () {
    // A visible size larger than the coded one cannot happen and must not be
    // acted on if it does: scaling below one would leave a band of nothing.
    final VideoCrop crop = VideoCrop.of(
      _encoding(
        codedWidth: 1280,
        codedHeight: 720,
        visibleWidth: 1920,
        visibleHeight: 0,
      ),
    );
    expect(crop, VideoCrop.whole);
  });

  testWidgets('a whole picture is drawn with no clip and no transform', (
    WidgetTester tester,
  ) async {
    await tester.pumpWidget(
      const Directionality(
        textDirection: TextDirection.ltr,
        child: VideoTextureView(textureId: 7, aspectRatio: 16 / 9),
      ),
    );
    expect(find.byType(ClipRect), findsNothing);
    expect(find.byType(Transform), findsNothing);
    expect(find.byType(Texture), findsOneWidget);
  });

  testWidgets('a cropped picture is enlarged about its top left corner', (
    WidgetTester tester,
  ) async {
    await tester.pumpWidget(
      const Directionality(
        textDirection: TextDirection.ltr,
        child: VideoTextureView(
          textureId: 7,
          aspectRatio: 16 / 9,
          crop: VideoCrop(scaleX: 1, scaleY: 1088 / 1080),
        ),
      ),
    );
    expect(find.byType(ClipRect), findsOneWidget);
    final Transform transform = tester.widget(find.byType(Transform));
    expect(transform.alignment, Alignment.topLeft);
    // The visible rows have to end up filling the box, so the row the picture
    // is drawn at must be the coded height over the visible one.
    expect(transform.transform.getMaxScaleOnAxis(), closeTo(1088 / 1080, 1e-12));
  });
}
