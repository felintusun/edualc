//! Moving the focus, or the view, to an element by its id once it is drawn.

use dioxus::prelude::*;

/// Runs `script` on the element with `id`, as `element`, once it is drawn; if it is there.
fn on_drawn(id: &str, script: &str) {
    document::eval(&format!(
        "requestAnimationFrame(() => {{
            const element = document.getElementById({id:?});
            if (element) {{ {script} }}
        }})"
    ));
}

/// The focus, the caret after the text if it is a text box.
pub const FOCUS_AT_END: &str = "element.focus();
    if (element.setSelectionRange) {
        element.setSelectionRange(element.value.length, element.value.length);
    }";

/// On a touch screen a text field the user didn't tap takes no focus: it would bring the
/// keyboard up over what just opened. It takes it as it is tapped.
const UNLESS_TYPED_ON_TOUCH: &str = "if (element.matches('input, textarea, [contenteditable]')
        && matchMedia('(pointer: coarse)').matches) { return; }";

/// Puts the focus on the element with `id` once it is drawn.
pub fn focus(id: &str) {
    on_drawn(id, "element.focus();");
}

/// Puts the focus on the text box with `id` once it is drawn, the cursor after its text.
pub fn focus_at_end(id: &str) {
    on_drawn(id, FOCUS_AT_END);
}

/// Puts the focus on the element with `id` as something opens by itself (a prompt): not on a
/// text field on a touch screen (see [`UNLESS_TYPED_ON_TOUCH`]).
pub fn grab_focus(id: &str) {
    on_drawn(id, &format!("{UNLESS_TYPED_ON_TOUCH} element.focus();"));
}

/// As [`grab_focus`], the caret after the text (a session opened, its draft in the composer).
pub fn grab_focus_at_end(id: &str) {
    on_drawn(id, &format!("{UNLESS_TYPED_ON_TOUCH} {FOCUS_AT_END}"));
}

/// Brings the element with `id` into view where a list has scrolled it out, no further than
/// that takes (a row chosen with the keys while the focus stays in a field).
pub fn reveal(id: &str) {
    on_drawn(id, "element.scrollIntoView({ block: 'nearest' });");
}
