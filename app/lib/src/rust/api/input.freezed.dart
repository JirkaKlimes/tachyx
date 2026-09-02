// GENERATED CODE - DO NOT MODIFY BY HAND
// coverage:ignore-file
// ignore_for_file: type=lint, type=warning, deprecated_member_use, deprecated_member_use_from_same_package
// ignore_for_file: unused_element, deprecated_member_use, deprecated_member_use_from_same_package, use_function_type_syntax_for_parameters, unnecessary_const, avoid_init_to_null, invalid_override_different_default_values_named, prefer_expression_function_bodies, annotate_overrides, invalid_annotation_target, unnecessary_question_mark

part of 'input.dart';

// **************************************************************************
// FreezedGenerator
// **************************************************************************

// GENERATED CODE - DO NOT MODIFY BY HAND
// dart format off
T _$identity<T>(T value) => value;
/// @nodoc
mixin _$InputEvent {





@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is InputEvent);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
    return 'InputEvent()';
}


}

/// @nodoc
class $InputEventCopyWith<$Res>  {
$InputEventCopyWith(InputEvent _, $Res Function(InputEvent) __);
}


/// Adds pattern-matching-related methods to [InputEvent].
extension InputEventPatterns on InputEvent {
/// A variant of `map` that fallback to returning `orElse`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeMap<TResult extends Object?>({TResult Function( InputEvent_PointerMotion value)?  pointerMotion,TResult Function( InputEvent_PointerMotionAbsolute value)?  pointerMotionAbsolute,TResult Function( InputEvent_PointerButton value)?  pointerButton,TResult Function( InputEvent_Wheel value)?  wheel,TResult Function( InputEvent_FingerScroll value)?  fingerScroll,TResult Function( InputEvent_FingerScrollEnd value)?  fingerScrollEnd,TResult Function( InputEvent_Key value)?  key,TResult Function( InputEvent_Text value)?  text,required TResult orElse(),}){
final _that = this;
switch (_that) {
case InputEvent_PointerMotion() when pointerMotion != null:
return pointerMotion(_that);case InputEvent_PointerMotionAbsolute() when pointerMotionAbsolute != null:
return pointerMotionAbsolute(_that);case InputEvent_PointerButton() when pointerButton != null:
return pointerButton(_that);case InputEvent_Wheel() when wheel != null:
return wheel(_that);case InputEvent_FingerScroll() when fingerScroll != null:
return fingerScroll(_that);case InputEvent_FingerScrollEnd() when fingerScrollEnd != null:
return fingerScrollEnd(_that);case InputEvent_Key() when key != null:
return key(_that);case InputEvent_Text() when text != null:
return text(_that);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// Callbacks receives the raw object, upcasted.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case final Subclass2 value:
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult map<TResult extends Object?>({required TResult Function( InputEvent_PointerMotion value)  pointerMotion,required TResult Function( InputEvent_PointerMotionAbsolute value)  pointerMotionAbsolute,required TResult Function( InputEvent_PointerButton value)  pointerButton,required TResult Function( InputEvent_Wheel value)  wheel,required TResult Function( InputEvent_FingerScroll value)  fingerScroll,required TResult Function( InputEvent_FingerScrollEnd value)  fingerScrollEnd,required TResult Function( InputEvent_Key value)  key,required TResult Function( InputEvent_Text value)  text,}){
final _that = this;
switch (_that) {
case InputEvent_PointerMotion():
return pointerMotion(_that);case InputEvent_PointerMotionAbsolute():
return pointerMotionAbsolute(_that);case InputEvent_PointerButton():
return pointerButton(_that);case InputEvent_Wheel():
return wheel(_that);case InputEvent_FingerScroll():
return fingerScroll(_that);case InputEvent_FingerScrollEnd():
return fingerScrollEnd(_that);case InputEvent_Key():
return key(_that);case InputEvent_Text():
return text(_that);}
}
/// A variant of `map` that fallback to returning `null`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? mapOrNull<TResult extends Object?>({TResult? Function( InputEvent_PointerMotion value)?  pointerMotion,TResult? Function( InputEvent_PointerMotionAbsolute value)?  pointerMotionAbsolute,TResult? Function( InputEvent_PointerButton value)?  pointerButton,TResult? Function( InputEvent_Wheel value)?  wheel,TResult? Function( InputEvent_FingerScroll value)?  fingerScroll,TResult? Function( InputEvent_FingerScrollEnd value)?  fingerScrollEnd,TResult? Function( InputEvent_Key value)?  key,TResult? Function( InputEvent_Text value)?  text,}){
final _that = this;
switch (_that) {
case InputEvent_PointerMotion() when pointerMotion != null:
return pointerMotion(_that);case InputEvent_PointerMotionAbsolute() when pointerMotionAbsolute != null:
return pointerMotionAbsolute(_that);case InputEvent_PointerButton() when pointerButton != null:
return pointerButton(_that);case InputEvent_Wheel() when wheel != null:
return wheel(_that);case InputEvent_FingerScroll() when fingerScroll != null:
return fingerScroll(_that);case InputEvent_FingerScrollEnd() when fingerScrollEnd != null:
return fingerScrollEnd(_that);case InputEvent_Key() when key != null:
return key(_that);case InputEvent_Text() when text != null:
return text(_that);case _:
  return null;

}
}
/// A variant of `when` that fallback to an `orElse` callback.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeWhen<TResult extends Object?>({TResult Function( double dx,  double dy)?  pointerMotion,TResult Function( double x,  double y)?  pointerMotionAbsolute,TResult Function( PointerButton button,  Press press)?  pointerButton,TResult Function( double dx,  double dy)?  wheel,TResult Function( double dx,  double dy)?  fingerScroll,TResult Function()?  fingerScrollEnd,TResult Function( HidUsage usage,  Press press)?  key,TResult Function( String field0)?  text,required TResult orElse(),}) {final _that = this;
switch (_that) {
case InputEvent_PointerMotion() when pointerMotion != null:
return pointerMotion(_that.dx,_that.dy);case InputEvent_PointerMotionAbsolute() when pointerMotionAbsolute != null:
return pointerMotionAbsolute(_that.x,_that.y);case InputEvent_PointerButton() when pointerButton != null:
return pointerButton(_that.button,_that.press);case InputEvent_Wheel() when wheel != null:
return wheel(_that.dx,_that.dy);case InputEvent_FingerScroll() when fingerScroll != null:
return fingerScroll(_that.dx,_that.dy);case InputEvent_FingerScrollEnd() when fingerScrollEnd != null:
return fingerScrollEnd();case InputEvent_Key() when key != null:
return key(_that.usage,_that.press);case InputEvent_Text() when text != null:
return text(_that.field0);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// As opposed to `map`, this offers destructuring.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case Subclass2(:final field2):
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult when<TResult extends Object?>({required TResult Function( double dx,  double dy)  pointerMotion,required TResult Function( double x,  double y)  pointerMotionAbsolute,required TResult Function( PointerButton button,  Press press)  pointerButton,required TResult Function( double dx,  double dy)  wheel,required TResult Function( double dx,  double dy)  fingerScroll,required TResult Function()  fingerScrollEnd,required TResult Function( HidUsage usage,  Press press)  key,required TResult Function( String field0)  text,}) {final _that = this;
switch (_that) {
case InputEvent_PointerMotion():
return pointerMotion(_that.dx,_that.dy);case InputEvent_PointerMotionAbsolute():
return pointerMotionAbsolute(_that.x,_that.y);case InputEvent_PointerButton():
return pointerButton(_that.button,_that.press);case InputEvent_Wheel():
return wheel(_that.dx,_that.dy);case InputEvent_FingerScroll():
return fingerScroll(_that.dx,_that.dy);case InputEvent_FingerScrollEnd():
return fingerScrollEnd();case InputEvent_Key():
return key(_that.usage,_that.press);case InputEvent_Text():
return text(_that.field0);}
}
/// A variant of `when` that fallback to returning `null`
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? whenOrNull<TResult extends Object?>({TResult? Function( double dx,  double dy)?  pointerMotion,TResult? Function( double x,  double y)?  pointerMotionAbsolute,TResult? Function( PointerButton button,  Press press)?  pointerButton,TResult? Function( double dx,  double dy)?  wheel,TResult? Function( double dx,  double dy)?  fingerScroll,TResult? Function()?  fingerScrollEnd,TResult? Function( HidUsage usage,  Press press)?  key,TResult? Function( String field0)?  text,}) {final _that = this;
switch (_that) {
case InputEvent_PointerMotion() when pointerMotion != null:
return pointerMotion(_that.dx,_that.dy);case InputEvent_PointerMotionAbsolute() when pointerMotionAbsolute != null:
return pointerMotionAbsolute(_that.x,_that.y);case InputEvent_PointerButton() when pointerButton != null:
return pointerButton(_that.button,_that.press);case InputEvent_Wheel() when wheel != null:
return wheel(_that.dx,_that.dy);case InputEvent_FingerScroll() when fingerScroll != null:
return fingerScroll(_that.dx,_that.dy);case InputEvent_FingerScrollEnd() when fingerScrollEnd != null:
return fingerScrollEnd();case InputEvent_Key() when key != null:
return key(_that.usage,_that.press);case InputEvent_Text() when text != null:
return text(_that.field0);case _:
  return null;

}
}

}

/// @nodoc


class InputEvent_PointerMotion extends InputEvent {
  const InputEvent_PointerMotion({required this.dx, required this.dy}): super._();
  

/// Horizontal displacement.
 final  double dx;
/// Vertical displacement.
 final  double dy;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$InputEvent_PointerMotionCopyWith<InputEvent_PointerMotion> get copyWith => _$InputEvent_PointerMotionCopyWithImpl<InputEvent_PointerMotion>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is InputEvent_PointerMotion&&(identical(other.dx, dx) || other.dx == dx)&&(identical(other.dy, dy) || other.dy == dy));
}


@override
int get hashCode {
    return Object.hash(runtimeType,dx,dy);
}

@override
String toString() {
    return 'InputEvent.pointerMotion(dx: $dx, dy: $dy)';
}


}

/// @nodoc
abstract mixin class $InputEvent_PointerMotionCopyWith<$Res> implements $InputEventCopyWith<$Res> {
  factory $InputEvent_PointerMotionCopyWith(InputEvent_PointerMotion value, $Res Function(InputEvent_PointerMotion) _then) = _$InputEvent_PointerMotionCopyWithImpl;
@useResult
$Res call({
 double dx, double dy
});




}
/// @nodoc
class _$InputEvent_PointerMotionCopyWithImpl<$Res>
    implements $InputEvent_PointerMotionCopyWith<$Res> {
  _$InputEvent_PointerMotionCopyWithImpl(this._self, this._then);

  final InputEvent_PointerMotion _self;
  final $Res Function(InputEvent_PointerMotion) _then;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? dx = null,Object? dy = null,}) {
  return _then(InputEvent_PointerMotion(
dx: null == dx ? _self.dx : dx // ignore: cast_nullable_to_non_nullable
as double,dy: null == dy ? _self.dy : dy // ignore: cast_nullable_to_non_nullable
as double,
  ));
}


}

/// @nodoc


class InputEvent_PointerMotionAbsolute extends InputEvent {
  const InputEvent_PointerMotionAbsolute({required this.x, required this.y}): super._();
  

/// Horizontal position, `0.0` at the left edge and `1.0` at the right.
 final  double x;
/// Vertical position, `0.0` at the top edge and `1.0` at the bottom.
 final  double y;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$InputEvent_PointerMotionAbsoluteCopyWith<InputEvent_PointerMotionAbsolute> get copyWith => _$InputEvent_PointerMotionAbsoluteCopyWithImpl<InputEvent_PointerMotionAbsolute>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is InputEvent_PointerMotionAbsolute&&(identical(other.x, x) || other.x == x)&&(identical(other.y, y) || other.y == y));
}


@override
int get hashCode {
    return Object.hash(runtimeType,x,y);
}

@override
String toString() {
    return 'InputEvent.pointerMotionAbsolute(x: $x, y: $y)';
}


}

/// @nodoc
abstract mixin class $InputEvent_PointerMotionAbsoluteCopyWith<$Res> implements $InputEventCopyWith<$Res> {
  factory $InputEvent_PointerMotionAbsoluteCopyWith(InputEvent_PointerMotionAbsolute value, $Res Function(InputEvent_PointerMotionAbsolute) _then) = _$InputEvent_PointerMotionAbsoluteCopyWithImpl;
@useResult
$Res call({
 double x, double y
});




}
/// @nodoc
class _$InputEvent_PointerMotionAbsoluteCopyWithImpl<$Res>
    implements $InputEvent_PointerMotionAbsoluteCopyWith<$Res> {
  _$InputEvent_PointerMotionAbsoluteCopyWithImpl(this._self, this._then);

  final InputEvent_PointerMotionAbsolute _self;
  final $Res Function(InputEvent_PointerMotionAbsolute) _then;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? x = null,Object? y = null,}) {
  return _then(InputEvent_PointerMotionAbsolute(
x: null == x ? _self.x : x // ignore: cast_nullable_to_non_nullable
as double,y: null == y ? _self.y : y // ignore: cast_nullable_to_non_nullable
as double,
  ));
}


}

/// @nodoc


class InputEvent_PointerButton extends InputEvent {
  const InputEvent_PointerButton({required this.button, required this.press}): super._();
  

/// Which button.
 final  PointerButton button;
/// Its new state.
 final  Press press;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$InputEvent_PointerButtonCopyWith<InputEvent_PointerButton> get copyWith => _$InputEvent_PointerButtonCopyWithImpl<InputEvent_PointerButton>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is InputEvent_PointerButton&&(identical(other.button, button) || other.button == button)&&(identical(other.press, press) || other.press == press));
}


@override
int get hashCode {
    return Object.hash(runtimeType,button,press);
}

@override
String toString() {
    return 'InputEvent.pointerButton(button: $button, press: $press)';
}


}

/// @nodoc
abstract mixin class $InputEvent_PointerButtonCopyWith<$Res> implements $InputEventCopyWith<$Res> {
  factory $InputEvent_PointerButtonCopyWith(InputEvent_PointerButton value, $Res Function(InputEvent_PointerButton) _then) = _$InputEvent_PointerButtonCopyWithImpl;
@useResult
$Res call({
 PointerButton button, Press press
});




}
/// @nodoc
class _$InputEvent_PointerButtonCopyWithImpl<$Res>
    implements $InputEvent_PointerButtonCopyWith<$Res> {
  _$InputEvent_PointerButtonCopyWithImpl(this._self, this._then);

  final InputEvent_PointerButton _self;
  final $Res Function(InputEvent_PointerButton) _then;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? button = null,Object? press = null,}) {
  return _then(InputEvent_PointerButton(
button: null == button ? _self.button : button // ignore: cast_nullable_to_non_nullable
as PointerButton,press: null == press ? _self.press : press // ignore: cast_nullable_to_non_nullable
as Press,
  ));
}


}

/// @nodoc


class InputEvent_Wheel extends InputEvent {
  const InputEvent_Wheel({required this.dx, required this.dy}): super._();
  

/// Horizontal distance, positive to the right.
 final  double dx;
/// Vertical distance, positive downwards.
 final  double dy;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$InputEvent_WheelCopyWith<InputEvent_Wheel> get copyWith => _$InputEvent_WheelCopyWithImpl<InputEvent_Wheel>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is InputEvent_Wheel&&(identical(other.dx, dx) || other.dx == dx)&&(identical(other.dy, dy) || other.dy == dy));
}


@override
int get hashCode {
    return Object.hash(runtimeType,dx,dy);
}

@override
String toString() {
    return 'InputEvent.wheel(dx: $dx, dy: $dy)';
}


}

/// @nodoc
abstract mixin class $InputEvent_WheelCopyWith<$Res> implements $InputEventCopyWith<$Res> {
  factory $InputEvent_WheelCopyWith(InputEvent_Wheel value, $Res Function(InputEvent_Wheel) _then) = _$InputEvent_WheelCopyWithImpl;
@useResult
$Res call({
 double dx, double dy
});




}
/// @nodoc
class _$InputEvent_WheelCopyWithImpl<$Res>
    implements $InputEvent_WheelCopyWith<$Res> {
  _$InputEvent_WheelCopyWithImpl(this._self, this._then);

  final InputEvent_Wheel _self;
  final $Res Function(InputEvent_Wheel) _then;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? dx = null,Object? dy = null,}) {
  return _then(InputEvent_Wheel(
dx: null == dx ? _self.dx : dx // ignore: cast_nullable_to_non_nullable
as double,dy: null == dy ? _self.dy : dy // ignore: cast_nullable_to_non_nullable
as double,
  ));
}


}

/// @nodoc


class InputEvent_FingerScroll extends InputEvent {
  const InputEvent_FingerScroll({required this.dx, required this.dy}): super._();
  

/// Horizontal distance, positive to the right.
 final  double dx;
/// Vertical distance, positive downwards.
 final  double dy;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$InputEvent_FingerScrollCopyWith<InputEvent_FingerScroll> get copyWith => _$InputEvent_FingerScrollCopyWithImpl<InputEvent_FingerScroll>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is InputEvent_FingerScroll&&(identical(other.dx, dx) || other.dx == dx)&&(identical(other.dy, dy) || other.dy == dy));
}


@override
int get hashCode {
    return Object.hash(runtimeType,dx,dy);
}

@override
String toString() {
    return 'InputEvent.fingerScroll(dx: $dx, dy: $dy)';
}


}

/// @nodoc
abstract mixin class $InputEvent_FingerScrollCopyWith<$Res> implements $InputEventCopyWith<$Res> {
  factory $InputEvent_FingerScrollCopyWith(InputEvent_FingerScroll value, $Res Function(InputEvent_FingerScroll) _then) = _$InputEvent_FingerScrollCopyWithImpl;
@useResult
$Res call({
 double dx, double dy
});




}
/// @nodoc
class _$InputEvent_FingerScrollCopyWithImpl<$Res>
    implements $InputEvent_FingerScrollCopyWith<$Res> {
  _$InputEvent_FingerScrollCopyWithImpl(this._self, this._then);

  final InputEvent_FingerScroll _self;
  final $Res Function(InputEvent_FingerScroll) _then;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? dx = null,Object? dy = null,}) {
  return _then(InputEvent_FingerScroll(
dx: null == dx ? _self.dx : dx // ignore: cast_nullable_to_non_nullable
as double,dy: null == dy ? _self.dy : dy // ignore: cast_nullable_to_non_nullable
as double,
  ));
}


}

/// @nodoc


class InputEvent_FingerScrollEnd extends InputEvent {
  const InputEvent_FingerScrollEnd(): super._();
  






@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is InputEvent_FingerScrollEnd);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
    return 'InputEvent.fingerScrollEnd()';
}


}




/// @nodoc


class InputEvent_Key extends InputEvent {
  const InputEvent_Key({required this.usage, required this.press}): super._();
  

/// Which key.
 final  HidUsage usage;
/// Its new state.
 final  Press press;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$InputEvent_KeyCopyWith<InputEvent_Key> get copyWith => _$InputEvent_KeyCopyWithImpl<InputEvent_Key>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is InputEvent_Key&&(identical(other.usage, usage) || other.usage == usage)&&(identical(other.press, press) || other.press == press));
}


@override
int get hashCode {
    return Object.hash(runtimeType,usage,press);
}

@override
String toString() {
    return 'InputEvent.key(usage: $usage, press: $press)';
}


}

/// @nodoc
abstract mixin class $InputEvent_KeyCopyWith<$Res> implements $InputEventCopyWith<$Res> {
  factory $InputEvent_KeyCopyWith(InputEvent_Key value, $Res Function(InputEvent_Key) _then) = _$InputEvent_KeyCopyWithImpl;
@useResult
$Res call({
 HidUsage usage, Press press
});




}
/// @nodoc
class _$InputEvent_KeyCopyWithImpl<$Res>
    implements $InputEvent_KeyCopyWith<$Res> {
  _$InputEvent_KeyCopyWithImpl(this._self, this._then);

  final InputEvent_Key _self;
  final $Res Function(InputEvent_Key) _then;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? usage = null,Object? press = null,}) {
  return _then(InputEvent_Key(
usage: null == usage ? _self.usage : usage // ignore: cast_nullable_to_non_nullable
as HidUsage,press: null == press ? _self.press : press // ignore: cast_nullable_to_non_nullable
as Press,
  ));
}


}

/// @nodoc


class InputEvent_Text extends InputEvent {
  const InputEvent_Text(this.field0): super._();
  

 final  String field0;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$InputEvent_TextCopyWith<InputEvent_Text> get copyWith => _$InputEvent_TextCopyWithImpl<InputEvent_Text>(this, _$identity);



@override
bool operator ==(Object other) {
    return identical(this, other) || (other.runtimeType == runtimeType&&other is InputEvent_Text&&(identical(other.field0, field0) || other.field0 == field0));
}


@override
int get hashCode {
    return Object.hash(runtimeType,field0);
}

@override
String toString() {
    return 'InputEvent.text(field0: $field0)';
}


}

/// @nodoc
abstract mixin class $InputEvent_TextCopyWith<$Res> implements $InputEventCopyWith<$Res> {
  factory $InputEvent_TextCopyWith(InputEvent_Text value, $Res Function(InputEvent_Text) _then) = _$InputEvent_TextCopyWithImpl;
@useResult
$Res call({
 String field0
});




}
/// @nodoc
class _$InputEvent_TextCopyWithImpl<$Res>
    implements $InputEvent_TextCopyWith<$Res> {
  _$InputEvent_TextCopyWithImpl(this._self, this._then);

  final InputEvent_Text _self;
  final $Res Function(InputEvent_Text) _then;

/// Create a copy of InputEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? field0 = null,}) {
  return _then(InputEvent_Text(
null == field0 ? _self.field0 : field0 // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

// dart format on
