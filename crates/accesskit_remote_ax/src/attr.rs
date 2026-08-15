//! The raw AXUIElement read boundary: every out-pointer and every CF cast in
//! this crate lives here, so nothing above it is `unsafe`.
//!
//! Two things shape this module. First, AX is *synchronous IPC into the target
//! application*, serviced on that app's main thread — so a read costs a round
//! trip and a busy app makes every read slow (the AT-SPI source measured the
//! same effect at 6-7s idle versus 79s busy for one LibreOffice walk). Hence
//! [`multiple`], which fetches a whole attribute set in one crossing, and
//! [`set_timeout`], without which an unresponsive app wedges the caller.
//!
//! Second, an absent attribute is *normal*, not exceptional: AX has no
//! interface set to gate reads on the way AT-SPI does, so asking a button for
//! its `AXValue` is the ordinary way to find out it has none. Every read here
//! therefore answers `Option`, and only genuinely unexpected failures surface
//! as an [`AxError`].

use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
};
use std::ptr::NonNull;

/// An AX call that failed for a reason the caller may want to react to, as
/// opposed to the routine "this element has no such attribute".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxError(pub AXError);

impl core::fmt::Display for AxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::error::Error for AxError {}

impl AxError {
    /// A stable short name, for logs and for the probe's per-app summary.
    /// `AXError`'s own `Debug` is a bare numeric newtype, which is unreadable
    /// in a dump of a few thousand reads.
    pub fn name(self) -> &'static str {
        match self.0 {
            AXError::Success => "success",
            AXError::Failure => "failure",
            AXError::IllegalArgument => "illegalArgument",
            AXError::InvalidUIElement => "invalidUIElement",
            AXError::InvalidUIElementObserver => "invalidUIElementObserver",
            AXError::CannotComplete => "cannotComplete",
            AXError::AttributeUnsupported => "attributeUnsupported",
            AXError::ActionUnsupported => "actionUnsupported",
            AXError::NotificationUnsupported => "notificationUnsupported",
            AXError::NotImplemented => "notImplemented",
            AXError::NotificationAlreadyRegistered => "notificationAlreadyRegistered",
            AXError::NotificationNotRegistered => "notificationNotRegistered",
            AXError::APIDisabled => "apiDisabled",
            AXError::NoValue => "noValue",
            AXError::ParameterizedAttributeUnsupported => "parameterizedAttributeUnsupported",
            AXError::NotEnoughPrecision => "notEnoughPrecision",
            _ => "unknown",
        }
    }

    /// Whether the element is gone or the app died, so the caller should drop
    /// it rather than retry. `CannotComplete` is the timeout/unresponsive case
    /// and is deliberately *not* included — that element may answer next time.
    pub fn is_element_dead(self) -> bool {
        matches!(self.0, AXError::InvalidUIElement)
    }
}

/// Splits an `AXError` into the three outcomes callers actually distinguish:
/// a value, a legitimate absence, or a real error.
fn classify<T>(error: AXError, value: Option<T>) -> Result<Option<T>, AxError> {
    match error {
        AXError::Success => Ok(value),
        // "No such attribute here" and "it exists but is empty" are both
        // ordinary shapes of an accessibility tree, not failures.
        AXError::AttributeUnsupported | AXError::NoValue | AXError::ActionUnsupported => Ok(None),
        other => Err(AxError(other)),
    }
}

/// Bounds every subsequent read of `element` (an application element, or the
/// system-wide element to set a process-global default).
///
/// Without this a single hung application blocks the calling thread
/// indefinitely — and on the AX side that thread is also the one running the
/// observer run loop, so one wedged app would stop *all* updates, not just its
/// own. The AT-SPI source has no equivalent guard because it drove its reads
/// from an async runtime where a stuck call blocked only one task.
pub fn set_timeout(element: &AXUIElement, seconds: f32) -> Result<(), AxError> {
    // SAFETY: `element` is a live AXUIElement; the call takes no pointers.
    let error = unsafe { element.set_messaging_timeout(seconds) };
    match error {
        AXError::Success => Ok(()),
        other => Err(AxError(other)),
    }
}

/// Reads one attribute as an untyped CF value.
pub fn value(
    element: &AXUIElement,
    attribute: &CFString,
) -> Result<Option<CFRetained<CFType>>, AxError> {
    let mut out: *const CFType = std::ptr::null();
    // SAFETY: `out` is a valid, writable pointer to a null-initialised slot.
    // The callee writes an owned (+1) reference into it only on success.
    let error = unsafe {
        element.copy_attribute_value(attribute, NonNull::from(&mut out))
    };
    let retained = NonNull::new(out.cast_mut())
        // SAFETY: on success AXUIElementCopyAttributeValue follows the CF Copy
        // rule and hands back a +1 reference, which CFRetained now owns.
        .map(|ptr| unsafe { CFRetained::from_raw(ptr) });
    match classify(error, retained) {
        Ok(value) => Ok(value),
        Err(e) => Err(e),
    }
}

/// Reads many attributes in **one** IPC round trip.
///
/// This is the AX analogue of the batching that took the AT-SPI walk from 8.1s
/// to 4.7s on a 2446-node tree, and it is a bigger win here: that was five
/// concurrent D-Bus calls, this is genuinely one crossing.
///
/// The returned vector is index-aligned with `attributes`; an entry is `None`
/// where the element has no such attribute. Values that failed individually
/// arrive as `AXValue`-wrapped error markers, which are reported as `None`
/// rather than guessed at.
pub fn multiple(
    element: &AXUIElement,
    attributes: &[CFRetained<CFString>],
) -> Result<Vec<Option<CFRetained<CFType>>>, AxError> {
    if attributes.is_empty() {
        return Ok(Vec::new());
    }
    let array = CFArray::from_retained_objects(attributes);
    let array = array.as_opaque();
    let mut out: *const CFArray = std::ptr::null();
    // SAFETY: `array` is a live CFArray of CFStrings and `out` is a valid
    // writable slot. Default options: individual failures come back as error
    // markers in the result array rather than failing the whole call.
    let error = unsafe {
        element.copy_multiple_attribute_values(
            array,
            objc2_application_services::AXCopyMultipleAttributeOptions::empty(),
            NonNull::from(&mut out),
        )
    };
    let values = NonNull::new(out.cast_mut())
        // SAFETY: +1 reference from a Copy-rule function.
        .map(|ptr| unsafe { CFRetained::<CFArray>::from_raw(ptr) });
    let Some(values) = classify(error, values)? else {
        return Ok(vec![None; attributes.len()]);
    };
    let mut result = Vec::with_capacity(attributes.len());
    for index in 0..attributes.len() {
        let item = array_get(&values, index);
        // An error marker is an AXValueRef of type AXError; treat it as absent.
        let item = item.filter(|value| !is_error_marker(value));
        result.push(item);
    }
    Ok(result)
}

/// Whether a `copy_multiple_attribute_values` slot holds an error marker
/// rather than a real value.
fn is_error_marker(value: &CFRetained<CFType>) -> bool {
    value
        .downcast_ref::<AXValue>()
        // SAFETY: `value` is a live AXValue.
        .is_some_and(|ax| unsafe { ax.r#type() } == AXValueType::AXError)
}

/// The names of every attribute this element exposes.
///
/// The characterization probe's core read: on AX there is no interface set to
/// enumerate, so the attribute list *is* the element's capability surface.
pub fn names(element: &AXUIElement) -> Result<Vec<String>, AxError> {
    let mut out: *const CFArray = std::ptr::null();
    // SAFETY: `out` is a valid writable slot; +1 reference on success.
    let error = unsafe { element.copy_attribute_names(NonNull::from(&mut out)) };
    let array = NonNull::new(out.cast_mut()).map(|ptr| unsafe { CFRetained::from_raw(ptr) });
    Ok(classify(error, array)?.map(|a| strings(&a)).unwrap_or_default())
}

/// The names of every action this element can perform.
pub fn action_names(element: &AXUIElement) -> Result<Vec<String>, AxError> {
    let mut out: *const CFArray = std::ptr::null();
    // SAFETY: `out` is a valid writable slot; +1 reference on success.
    let error = unsafe { element.copy_action_names(NonNull::from(&mut out)) };
    let array = NonNull::new(out.cast_mut()).map(|ptr| unsafe { CFRetained::from_raw(ptr) });
    Ok(classify(error, array)?.map(|a| strings(&a)).unwrap_or_default())
}

/// Whether an attribute can be written.
///
/// AX's advantage over AT-SPI: a real capability probe. The AT-SPI drive path
/// had to attempt a call and interpret `NotSupported` afterwards, spending a
/// round trip per guess; here the plan can be pruned before any write.
pub fn is_settable(element: &AXUIElement, attribute: &CFString) -> Result<bool, AxError> {
    let mut settable: u8 = 0;
    // SAFETY: `settable` is a valid writable byte, which is `Boolean`'s layout.
    let error = unsafe { element.is_attribute_settable(attribute, NonNull::from(&mut settable)) };
    Ok(classify(error, Some(settable != 0))?.unwrap_or(false))
}

/// The process that owns this element.
pub fn pid(element: &AXUIElement) -> Result<i32, AxError> {
    let mut pid: libc::pid_t = 0;
    // SAFETY: `pid` is a valid writable slot.
    let error = unsafe { element.pid(NonNull::from(&mut pid)) };
    match error {
        AXError::Success => Ok(pid),
        other => Err(AxError(other)),
    }
}

// ---------------------------------------------------------------- typed reads

/// Reads an attribute expected to hold a string. A present-but-wrongly-typed
/// value reads as absent rather than as an error: toolkits do put surprising
/// types in these slots, and one odd node should not fail a whole walk.
pub fn string(element: &AXUIElement, attribute: &CFString) -> Result<Option<String>, AxError> {
    Ok(value(element, attribute)?.and_then(|v| as_string(&v)))
}

/// Reads an attribute expected to hold another element.
pub fn element(
    element: &AXUIElement,
    attribute: &CFString,
) -> Result<Option<CFRetained<AXUIElement>>, AxError> {
    Ok(value(element, attribute)?.and_then(as_element))
}

/// Reads an attribute expected to hold an array of elements.
pub fn elements(
    element: &AXUIElement,
    attribute: &CFString,
) -> Result<Vec<CFRetained<AXUIElement>>, AxError> {
    Ok(value(element, attribute)?
        .map(|v| as_elements(&v))
        .unwrap_or_default())
}

/// Reads an attribute expected to hold a boolean.
pub fn boolean(element: &AXUIElement, attribute: &CFString) -> Result<Option<bool>, AxError> {
    Ok(value(element, attribute)?.and_then(|v| as_bool(&v)))
}

// -------------------------------------------------------------- CF conversion
//
// Kept as free functions over `&CFType` so the batched path can reuse them: a
// `multiple` result is already-fetched values that still need the same casts.

pub fn as_string(value: &CFType) -> Option<String> {
    value.downcast_ref::<CFString>().map(|s| s.to_string())
}

pub fn as_bool(value: &CFType) -> Option<bool> {
    if let Some(b) = value.downcast_ref::<CFBoolean>() {
        return Some(b.as_bool());
    }
    // Several toolkits report boolean-ish state as 0/1 numbers.
    as_f64(value).map(|n| n != 0.0)
}

pub fn as_f64(value: &CFType) -> Option<f64> {
    value.downcast_ref::<CFNumber>().and_then(|n| n.as_f64())
}

pub fn as_element(value: CFRetained<CFType>) -> Option<CFRetained<AXUIElement>> {
    value.downcast::<AXUIElement>().ok()
}

pub fn as_elements(value: &CFType) -> Vec<CFRetained<AXUIElement>> {
    let Some(array) = value.downcast_ref::<CFArray>() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(array.count().max(0) as usize);
    for index in 0..array.count().max(0) as usize {
        if let Some(item) = array_get(array, index).and_then(as_element) {
            out.push(item);
        }
    }
    out
}

/// Unwraps an `AXValue` holding a `CGPoint`.
pub fn as_point(value: &CFType) -> Option<CGPoint> {
    let ax = value.downcast_ref::<AXValue>()?;
    // SAFETY: the type tag is checked before reading, and `point` is a valid
    // writable CGPoint the callee fills in.
    unsafe {
        if ax.r#type() != AXValueType::CGPoint {
            return None;
        }
        let mut point = CGPoint::new(0.0, 0.0);
        let ptr = NonNull::from(&mut point).cast();
        ax.value(AXValueType::CGPoint, ptr).then_some(point)
    }
}

/// Unwraps an `AXValue` holding a `CGRect`.
///
/// `AXFrame` is present on 100% of elements surveyed on a real desktop and
/// carries position and size together, so preferring it over `AXPosition` plus
/// `AXSize` halves the geometry cost per node.
pub fn as_rect(value: &CFType) -> Option<CGRect> {
    let ax = value.downcast_ref::<AXValue>()?;
    // SAFETY: the type tag is checked before reading, and `rect` is a valid
    // writable CGRect the callee fills in.
    unsafe {
        if ax.r#type() != AXValueType::CGRect {
            return None;
        }
        let mut rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0));
        let ptr = NonNull::from(&mut rect).cast();
        ax.value(AXValueType::CGRect, ptr).then_some(rect)
    }
}

/// Unwraps an `AXValue` holding a `CFRange`, as `AXSelectedTextRange` does.
///
/// The units are UTF-16 code units; see [`crate::text`] for why that matters.
pub fn as_range(value: &CFType) -> Option<(usize, usize)> {
    let ax = value.downcast_ref::<AXValue>()?;
    // SAFETY: the type tag is checked before reading, and `range` is a valid
    // writable CFRange the callee fills in.
    unsafe {
        if ax.r#type() != AXValueType::CFRange {
            return None;
        }
        let mut range = objc2_core_foundation::CFRange { location: 0, length: 0 };
        let ptr = NonNull::from(&mut range).cast();
        ax.value(AXValueType::CFRange, ptr).then(|| {
            (range.location.max(0) as usize, range.length.max(0) as usize)
        })
    }
}

/// Unwraps an `AXValue` holding a `CGSize`.
pub fn as_size(value: &CFType) -> Option<CGSize> {
    let ax = value.downcast_ref::<AXValue>()?;
    // SAFETY: as `as_point`.
    unsafe {
        if ax.r#type() != AXValueType::CGSize {
            return None;
        }
        let mut size = CGSize::new(0.0, 0.0);
        let ptr = NonNull::from(&mut size).cast();
        ax.value(AXValueType::CGSize, ptr).then_some(size)
    }
}

/// Every string in a CF array, skipping entries that are not strings.
pub fn strings(array: &CFArray) -> Vec<String> {
    let mut out = Vec::with_capacity(array.count().max(0) as usize);
    for index in 0..array.count().max(0) as usize {
        if let Some(item) = array_get(array, index).as_deref().and_then(as_string) {
            out.push(item);
        }
    }
    out
}

/// Bounds-checked, retained element access.
///
/// `CFArrayGetValueAtIndex` follows the Get rule (no ownership transfer) and
/// traps rather than returning null when the index is out of range, so the
/// bound is checked here and the borrowed value is retained before it escapes.
fn array_get(array: &CFArray, index: usize) -> Option<CFRetained<CFType>> {
    let count = array.count();
    if count <= 0 || index >= count as usize {
        return None;
    }
    // SAFETY: `index` is in bounds, checked above. The returned pointer is a
    // borrow (+0), so it is retained before being handed out as owned.
    unsafe {
        let ptr = array.value_at_index(index as isize);
        NonNull::new(ptr.cast_mut()).map(|ptr| CFRetained::retain(ptr.cast::<CFType>()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These need no live element: CF value construction and the casts over it
    // are what the walk's correctness rests on, and they are testable with no
    // Accessibility grant, no session, and no running application — which is
    // what lets them run in CI.

    #[test]
    fn casts_read_their_own_type_and_decline_others() {
        let string = CFString::from_static_str("AXButton");
        assert_eq!(as_string(&string).as_deref(), Some("AXButton"));
        assert_eq!(as_bool(&string), None);
        assert_eq!(as_f64(&string), None);
        assert!(as_point(&string).is_none());

        let number = CFNumber::new_i32(3);
        assert_eq!(as_f64(&number), Some(3.0));
        assert_eq!(as_string(&number), None);
    }

    #[test]
    fn a_numeric_zero_or_one_reads_as_a_boolean() {
        // AppKit reports `AXEnabled` as a CFBoolean, but several toolkits use
        // 0/1 numbers for the same idea, so both must map.
        assert_eq!(as_bool(CFBoolean::new(true)), Some(true));
        assert_eq!(as_bool(CFBoolean::new(false)), Some(false));
        assert_eq!(as_bool(&CFNumber::new_i32(1)), Some(true));
        assert_eq!(as_bool(&CFNumber::new_i32(0)), Some(false));
    }

    #[test]
    fn array_access_is_bounds_checked() {
        let items = [
            CFString::from_static_str("a"),
            CFString::from_static_str("b"),
        ];
        let array = CFArray::from_retained_objects(&items);
        let array = array.as_opaque();
        assert_eq!(array_get(array, 0).as_deref().and_then(as_string).as_deref(), Some("a"));
        assert_eq!(array_get(array, 1).as_deref().and_then(as_string).as_deref(), Some("b"));
        // Past the end must answer None rather than trapping in CoreFoundation.
        assert!(array_get(array, 2).is_none());
        assert!(array_get(array, usize::MAX).is_none());
        assert_eq!(strings(array), vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn an_empty_array_yields_nothing() {
        // The case that matters most: AX returns an empty `AXWindows` array for
        // an application with no windows, and reading index 0 of it would hand
        // back whatever CoreFoundation had lying there.
        let empty: [CFRetained<CFType>; 0] = [];
        let array = CFArray::from_retained_objects(&empty);
        let array = array.as_opaque();
        assert_eq!(array.count(), 0, "an empty array must report zero entries");
        assert!(array_get(array, 0).is_none(), "index 0 of an empty array is not a value");
        assert!(strings(array).is_empty());
    }

    #[test]
    fn non_string_entries_are_skipped_rather_than_faked() {
        let items: [CFRetained<CFType>; 3] = [
            CFString::from_static_str("a").into(),
            CFNumber::new_i32(7).into(),
            CFString::from_static_str("b").into(),
        ];
        let array = CFArray::from_retained_objects(&items);
        assert_eq!(strings(array.as_opaque()), vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn ax_point_and_size_round_trip_through_their_wrapper() {
        let point = CGPoint::new(12.0, 34.5);
        // SAFETY: the pointer is a valid CGPoint matching the declared type.
        let wrapped = unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&point).cast()) }
            .expect("AXValue wrapping a CGPoint");
        let read = as_point(&wrapped).expect("reads back as a point");
        assert_eq!((read.x, read.y), (12.0, 34.5));

        let size = CGSize::new(80.0, 22.0);
        // SAFETY: as above, for CGSize.
        let wrapped = unsafe { AXValue::new(AXValueType::CGSize, NonNull::from(&size).cast()) }
            .expect("AXValue wrapping a CGSize");
        assert!(as_point(&wrapped).is_none(), "a size is not a point");
        let read = as_size(&wrapped).expect("reads back as a size");
        assert_eq!((read.width, read.height), (80.0, 22.0));
    }

    #[test]
    fn a_frame_reads_back_and_is_not_confused_with_a_point() {
        let rect = CGRect::new(CGPoint::new(10.0, 20.0), CGSize::new(300.0, 40.0));
        // SAFETY: the pointer is a valid CGRect matching the declared type.
        let wrapped = unsafe { AXValue::new(AXValueType::CGRect, NonNull::from(&rect).cast()) }
            .expect("AXValue wrapping a CGRect");
        let read = as_rect(&wrapped).expect("reads back as a rect");
        assert_eq!((read.origin.x, read.origin.y), (10.0, 20.0));
        assert_eq!((read.size.width, read.size.height), (300.0, 40.0));
        assert!(as_point(&wrapped).is_none(), "a rect is not a point");
        assert!(as_size(&wrapped).is_none(), "nor a size");
    }

    #[test]
    fn error_names_are_stable_and_death_is_narrow() {
        assert_eq!(AxError(AXError::AttributeUnsupported).name(), "attributeUnsupported");
        assert_eq!(AxError(AXError::APIDisabled).name(), "apiDisabled");
        assert!(AxError(AXError::InvalidUIElement).is_element_dead());
        // A timeout must not be mistaken for a dead element: the app is busy,
        // and the element will very likely answer on a later pass.
        assert!(!AxError(AXError::CannotComplete).is_element_dead());
    }

    #[test]
    fn absent_attributes_are_not_errors() {
        assert_eq!(classify::<u8>(AXError::AttributeUnsupported, None), Ok(None));
        assert_eq!(classify::<u8>(AXError::NoValue, None), Ok(None));
        assert_eq!(classify(AXError::Success, Some(1u8)), Ok(Some(1)));
        assert_eq!(
            classify::<u8>(AXError::CannotComplete, None),
            Err(AxError(AXError::CannotComplete)),
            "a timeout is a real failure the caller may want to report"
        );
    }
}
