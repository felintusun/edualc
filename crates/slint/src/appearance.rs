//! How text looks: Fira where it is installed, at the size the desktop sets.

#[cfg(not(target_arch = "wasm32"))]
use slint::fontique_012::fontique::{Collection, GenericFamily};

/// The interface's and the code's families, in order of preference.
const SANS: &[&str] = &["Fira Sans"];
const MONO: &[&str] = &["Fira Code", "Fira Mono"];

/// Points to logical pixels: the window scales those for the screen (1.5 on a HiDPI laptop).
const PIXELS_PER_POINT: f32 = 96.0 / 72.0;
/// Where the desktop says nothing: 11pt, the usual desktop default.
const DEFAULT_POINTS: f32 = 11.0;

#[derive(Debug, Default)]
pub(crate) struct Fonts {
    pub(crate) sans: Option<String>,
    pub(crate) mono: Option<String>,
}

/// Returns Fira if it is installed, else the desktop's sans and monospace.
///
/// Slint's font collection belongs to the thread that draws: call this there.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn fonts() -> Fonts {
    let mut collection = slint::fontique_012::shared_collection();
    Fonts {
        sans: pick(&mut collection, SANS, GenericFamily::SansSerif),
        mono: pick(&mut collection, MONO, GenericFamily::Monospace),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn pick(collection: &mut Collection, preferred: &[&str], generic: GenericFamily) -> Option<String> {
    if let Some(family) = preferred
        .iter()
        .find(|family| collection.family_id(family).is_some())
    {
        return Some((*family).to_owned());
    }
    let id = collection.generic_families(generic).next()?;
    collection.family_name(id).map(str::to_owned)
}

/// Returns the desktop's text size in logical pixels: GNOME's (and GTK's) interface font and
/// text scaling, or KDE's general font, or 11pt.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn desktop_text_size() -> f32 {
    gnome_points()
        .or_else(kde_points)
        .map_or(DEFAULT_TEXT_SIZE, |points| points * PIXELS_PER_POINT)
}

/// The text size where no desktop says one (the browser demo's), in logical pixels.
pub(crate) const DEFAULT_TEXT_SIZE: f32 = DEFAULT_POINTS * PIXELS_PER_POINT;

/// GNOME's interface font ("Adwaita Sans 11") times its text scaling factor, as the desktop's
/// Settings portal reads them.
#[cfg(not(target_arch = "wasm32"))]
fn gnome_points() -> Option<f32> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .inspect_err(|error| tracing::debug!(%error, "no runtime to ask the desktop"))
        .ok()?;
    runtime.block_on(async {
        const INTERFACE: &str = "org.gnome.desktop.interface";
        let settings = ashpd::desktop::settings::Settings::new()
            .await
            .inspect_err(|error| tracing::debug!(%error, "no Settings portal"))
            .ok()?;
        let font: String = settings
            .read(INTERFACE, "font-name")
            .await
            .inspect_err(|error| tracing::debug!(%error, "the portal has no GNOME font"))
            .ok()?;
        let points = font_points(&font)?;
        // The factor is a double; its decimal parses to the nearest f32, without a cast.
        let scaling = settings
            .read::<f64>(INTERFACE, "text-scaling-factor")
            .await
            .ok()
            .and_then(|factor| factor.to_string().parse::<f32>().ok())
            .filter(|factor| *factor > 0.0)
            .unwrap_or(1.0);
        Some(points * scaling)
    })
}

/// Reads the size at the end of a font name, as GNOME writes it: "Adwaita Sans 11".
#[cfg(not(target_arch = "wasm32"))]
fn font_points(font: &str) -> Option<f32> {
    font.rsplit(' ')
        .next()?
        .parse()
        .ok()
        .filter(|points: &f32| *points > 0.0)
}

/// KDE's general font: `font=Noto Sans,10,-1,5,…` under `[General]` in `kdeglobals`.
#[cfg(not(target_arch = "wasm32"))]
fn kde_points() -> Option<f32> {
    let config = std::fs::read_to_string(dirs::config_dir()?.join("kdeglobals")).ok()?;
    let mut in_general = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_general = line == "[General]";
        } else if in_general && let Some(font) = line.strip_prefix("font=") {
            return font.split(',').nth(1)?.parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_font_names_end_in_their_size() {
        assert_eq!(font_points("Adwaita Sans 11"), Some(11.0));
        assert_eq!(font_points("Cantarell 10.5"), Some(10.5));
        assert_eq!(font_points("Sans"), None);
    }
}
