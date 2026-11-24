//! The Slint window as a frontend: the presentation model set into its globals, and its
//! callbacks made the app's actions.

use std::collections::{HashMap, HashSet};

use slint::{
    ComponentHandle, Model, ModelExt, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString,
    VecModel,
};

use crate::chat::ImageKey;
use crate::colourist::{self, Coloured, Colourist, Order, Orders, Place};
use crate::decode::Pixels;
use crate::generated::{self as slint_ui, AppWindow};
use crate::present::model as present;
use crate::present::{Command, EntriesChange, Frontend, Image, Look, Styled};

mod callbacks;
mod model;

pub(crate) use callbacks::wire;

/// What the presentation model's values are drawn with: the conversation's decoded images,
/// and code's colours, which a thread of their own makes (`colourist`). Code not coloured yet
/// is shown plain.
pub(crate) struct Drawing {
    /// The images decoded so far.
    images: HashMap<ImageKey, slint::Image>,
    /// Where code is sent to be coloured.
    orders: Orders,
    /// Counts the sets of rows and colours; a new session or new colours make a new one, and
    /// colours made for an older one are dropped.
    generation: u64,
    /// Coloured code by its text, with its language.
    coloured: HashMap<String, (String, slint::StyledText)>,
    /// The code last coloured at each place. A block streaming in grows a line at a time, so
    /// the next code coloured at a place replaces the last.
    placed: HashMap<Place, String>,
}

impl Drawing {
    fn new(orders: Orders) -> Self {
        Self {
            images: HashMap::new(),
            orders,
            generation: 0,
            coloured: HashMap::new(),
            placed: HashMap::new(),
        }
    }

    /// Returns `code` in `lang`'s colours if it is coloured yet, else plain.
    fn code(&self, lang: &str, code: &str) -> slint::StyledText {
        self.coloured_as(lang, code).unwrap_or_else(|| plain(code))
    }

    /// Returns `code` in `lang`'s colours, if it is coloured yet.
    fn coloured_as(&self, lang: &str, code: &str) -> Option<slint::StyledText> {
        match self.coloured.get(code) {
            Some((coloured_as, coloured)) if coloured_as == lang => Some(coloured.clone()),
            Some(_) | None => None,
        }
    }

    /// Sends `block` at `place` to be coloured, unless it is coloured already or isn't code to
    /// colour.
    fn order(&self, place: Place, block: &present::MdBlock) {
        if !is_coloured_code(block) || self.coloured_as(&block.lang, &block.raw).is_some() {
            return;
        }
        self.orders.send(Order::Colour(colourist::Job {
            generation: self.generation,
            place,
            lang: block.lang.clone(),
            raw: block.raw.clone(),
        }));
    }

    /// Starts a new generation for other rows or colours, dropping what was coloured and what
    /// is being coloured.
    fn renew(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.coloured.clear();
        self.placed.clear();
    }

    /// Keeps `done` for its code to show, if it is of this generation; returns whether it is.
    fn take(&mut self, done: &Coloured) -> bool {
        let job = &done.job;
        if job.generation != self.generation {
            return false;
        }
        if let Some(replaced) = self.placed.insert(job.place, job.raw.clone())
            && replaced != job.raw
            // The same code may be shown at another place, which still draws it from here.
            && !self.placed.values().any(|shown| *shown == replaced)
        {
            self.coloured.remove(&replaced);
        }
        self.coloured
            .insert(job.raw.clone(), (job.lang.clone(), done.text.clone()));
        true
    }

    /// Sends the code of the side panel and the request bar's plan to be coloured.
    fn order_side(&self, side: Option<&present::Side>, prompt: Option<&present::Prompt>) {
        for (exchange, shown) in side
            .iter()
            .flat_map(|side| side.exchanges.iter().enumerate())
        {
            for (block, md) in shown.answer.iter().enumerate() {
                self.order(Place::Side { exchange, block }, md);
            }
        }
        for (section, shown) in prompt
            .iter()
            .flat_map(|prompt| prompt.plan.iter().enumerate())
        {
            self.order(Place::Plan { section }, &shown.block);
        }
    }
}

/// Sends the code of row `row` to be coloured where `new` differs from `old`, or all of it if
/// there is no `old`.
fn order_changed(
    drawing: &Drawing,
    row: usize,
    old: Option<&present::Entry>,
    new: &present::Entry,
) {
    for (block, md) in new.blocks.iter().enumerate() {
        if old.and_then(|old| old.blocks.get(block)) != Some(md) {
            drawing.order(Place::Entry { row, block }, md);
        }
    }
}

/// Returns whether `block` is code to colour, which is code that names its language. Code that
/// names none is always shown plain.
fn is_coloured_code(block: &present::MdBlock) -> bool {
    block.kind == present::BlockKind::Code && !block.lang.trim().is_empty()
}

/// Returns whether code growing from `old` to `new` keeps showing its old colours until its
/// new ones come, rather than plain meanwhile; its newest lines show a moment late. Only code
/// to colour does, as for other code the new colours never come.
fn keeps_colours(old: &present::MdBlock, new: &present::MdBlock, drawing: &Drawing) -> bool {
    is_coloured_code(new)
        && old.kind == present::BlockKind::Code
        && old.lang == new.lang
        && new.raw.starts_with(old.raw.as_str())
        && drawing.coloured_as(&new.lang, &new.raw).is_none()
}

/// Returns whether `md`, shown now, is drawn with what `job` coloured: it is the same code, or
/// code grown from it whose own colours haven't come. The second case means a block streaming
/// in faster than its colours come is drawn as far as they go, rather than left as it was when
/// the first came (see [`keeps_colours`]).
fn draws(md: &present::MdBlock, job: &colourist::Job, drawing: &Drawing) -> bool {
    md.kind == present::BlockKind::Code
        && md.lang == job.lang
        && (md.raw == job.raw
            || (md.raw.starts_with(job.raw.as_str())
                && drawing.coloured_as(&md.lang, &md.raw).is_none()))
}

/// Returns a Markdown block's body as the Slint window draws it: text with the space between
/// paragraphs that Slint leaves out, and code in colour once it is coloured.
fn md_body(block: &present::MdBlock, drawing: &Drawing) -> slint::StyledText {
    match (block.kind, &block.body) {
        (present::BlockKind::Text, Styled::Markdown(text)) => {
            Styled::Markdown(crate::markdown::spaced_for_slint(text)).to_slint(drawing)
        }
        (present::BlockKind::Code, _) => drawing.code(&block.lang, &block.raw),
        _ => block.body.to_slint(drawing),
    }
}

/// A value of the presentation model as Slint's.
pub(crate) trait ToSlint {
    type Slint: Clone + 'static;
    fn to_slint(&self, images: &Drawing) -> Self::Slint;
    /// Returns this value in place of `old`, shown as `shown`; lists in it keep their models.
    fn sync(&self, _old: &Self, _shown: Self::Slint, images: &Drawing) -> Self::Slint {
        self.to_slint(images)
    }
}

impl ToSlint for String {
    type Slint = SharedString;
    fn to_slint(&self, _: &Drawing) -> SharedString {
        self.as_str().into()
    }
}

impl ToSlint for bool {
    type Slint = Self;
    fn to_slint(&self, _: &Drawing) -> Self {
        *self
    }
}

impl ToSlint for i32 {
    type Slint = Self;
    fn to_slint(&self, _: &Drawing) -> Self {
        *self
    }
}

impl ToSlint for f32 {
    type Slint = Self;
    fn to_slint(&self, _: &Drawing) -> Self {
        *self
    }
}

impl ToSlint for Styled {
    type Slint = slint::StyledText;
    fn to_slint(&self, _: &Drawing) -> slint::StyledText {
        match self {
            Self::Empty => slint::StyledText::default(),
            Self::Plain(text) => plain(text),
            Self::Markdown(text) => styled(text),
        }
    }
}

impl ToSlint for Image {
    type Slint = slint::Image;
    fn to_slint(&self, images: &Drawing) -> slint::Image {
        match self {
            Self::None => slint::Image::default(),
            Self::Key(key) => images.images.get(key).cloned().unwrap_or_default(),
            Self::Pixels(pixels) => slint_image(pixels).unwrap_or_default(),
        }
    }
}

impl<T: ToSlint + PartialEq> ToSlint for Vec<T> {
    type Slint = ModelRc<T::Slint>;
    fn to_slint(&self, images: &Drawing) -> Self::Slint {
        let rows: Vec<T::Slint> = self.iter().map(|row| row.to_slint(images)).collect();
        ModelRc::new(VecModel::from(rows))
    }
    fn sync(&self, old: &Self, shown: Self::Slint, images: &Drawing) -> Self::Slint {
        resync(&shown, old, self, images).unwrap_or(shown)
    }
}

/// Brings `model`, showing `old`, in line with `new` row by row. A row that differs is synced
/// in place, so its own lists keep their models; rows are added or taken off at the end. A
/// model that isn't a `VecModel`, which none made here is, is replaced.
pub(crate) fn resync<T: ToSlint + PartialEq>(
    model: &ModelRc<T::Slint>,
    old: &[T],
    new: &[T],
    images: &Drawing,
) -> Option<ModelRc<T::Slint>> {
    let Some(rows) = model.as_any().downcast_ref::<VecModel<T::Slint>>() else {
        return Some(new.to_vec_slint(images));
    };
    if rows.row_count() != old.len() {
        // Not what was shown: anew.
        rows.set_vec(
            new.iter()
                .map(|row| row.to_slint(images))
                .collect::<Vec<_>>(),
        );
        return None;
    }
    for (index, row) in new.iter().enumerate() {
        match (old.get(index), rows.row_data(index)) {
            (Some(was), Some(_)) if was == row => {}
            (Some(was), Some(shown)) => rows.set_row_data(index, row.sync(was, shown, images)),
            (None, _) | (Some(_), None) => rows.push(row.to_slint(images)),
        }
    }
    while rows.row_count() > new.len() {
        rows.remove(rows.row_count() - 1);
    }
    None
}

trait ToVecSlint<T: ToSlint> {
    fn to_vec_slint(&self, images: &Drawing) -> ModelRc<T::Slint>;
}

impl<T: ToSlint> ToVecSlint<T> for [T] {
    fn to_vec_slint(&self, images: &Drawing) -> ModelRc<T::Slint> {
        let rows: Vec<T::Slint> = self.iter().map(|row| row.to_slint(images)).collect();
        ModelRc::new(VecModel::from(rows))
    }
}

/// The Slint window, as the presenter's frontend.
pub(crate) struct SlintFrontend {
    window: AppWindow,
    drawing: Drawing,
    /// The thread that colours code, which wakes the window when code is coloured.
    colourist: Colourist,
    /// The transcript's rows: the model the list filters, and what the rows show.
    entries: ModelRc<slint_ui::Entry>,
    shown: Vec<present::Entry>,
    /// What the side panel and the request bar were set to last, to colour their code again.
    prompt: Option<present::Prompt>,
    side: Option<present::Side>,
    #[cfg(not(target_arch = "wasm32"))]
    desk: crate::seat::Desk,
}

impl SlintFrontend {
    pub(crate) fn new(
        window: AppWindow,
        fonts: crate::appearance::Fonts,
        events: async_channel::Sender<crate::app::Event>,
    ) -> Self {
        if let Some(family) = fonts.sans {
            window.global::<slint_ui::Theme>().set_sans(family.into());
        }
        if let Some(family) = fonts.mono {
            set_monospace(&family);
            window.global::<slint_ui::Theme>().set_mono(family.into());
        }
        let entries = ModelRc::new(VecModel::<slint_ui::Entry>::default());
        // The list gets only the rows it shows. Slint's list estimates the rows it hasn't built
        // from those it has, so a row that takes no room misleads it: hidden rows past the end
        // put the end further down than it is, and following the end never settles; a view of
        // hidden rows alone measures nothing and is built again on every pass ("long
        // callback/instantiation chain detected").
        window
            .global::<slint_ui::Transcript>()
            .set_entries(ModelRc::new(entries.clone().filter(|entry| !entry.hidden)));
        #[cfg(not(target_arch = "wasm32"))]
        let desk = crate::seat::Desk::start(events.clone());
        let (orders, colourist) =
            colourist::spawn(&window.global::<slint_ui::Theme>().get_syntax(), events);
        Self {
            window,
            drawing: Drawing::new(orders),
            colourist,
            entries,
            shown: Vec::new(),
            prompt: None,
            side: None,
            #[cfg(not(target_arch = "wasm32"))]
            desk,
        }
    }

    /// Colours the code shown again, after the look or the desktop's dark or light changed. It
    /// keeps its colours and selection until the new colours come.
    fn recolour_code(&mut self) {
        self.drawing.renew();
        self.drawing.orders.send(Order::Colours {
            generation: self.drawing.generation,
            colours: self.window.global::<slint_ui::Theme>().get_syntax(),
        });
        self.order_all();
    }

    /// Sends all code shown to be coloured, the transcript's from its end, which shows first.
    fn order_all(&self) {
        for (row, entry) in self.shown.iter().enumerate().rev() {
            order_changed(&self.drawing, row, None, entry);
        }
        self.drawing
            .order_side(self.side.as_ref(), self.prompt.as_ref());
    }

    /// Draws the code coloured since the last call, where it is still shown as it was coloured
    /// or has only grown. The side panel and the plan are drawn again whole, each only if its
    /// own code was coloured. Code that this draws plain, because its colours were made for
    /// another place and dropped there since, is sent to be coloured again.
    fn draw_coloured(&mut self) {
        let (mut side_coloured, mut plan_coloured) = (false, false);
        for done in self.colourist.take() {
            if !self.drawing.take(&done) {
                continue;
            }
            match done.job.place {
                Place::Entry { row, block } => self.draw_in_row(row, block, &done),
                Place::Side { .. } => side_coloured = true,
                Place::Plan { .. } => plan_coloured = true,
            }
        }
        let side = self.side.as_ref().filter(|_| side_coloured);
        let prompt = self.prompt.as_ref().filter(|_| plan_coloured);
        if let Some(side) = side {
            model::apply_side(&self.window, None, side, &self.drawing);
        }
        if let Some(prompt) = prompt {
            model::apply_prompt(&self.window, None, prompt, &self.drawing);
        }
        self.drawing.order_side(side, prompt);
    }

    /// Shows `done` as block `block` of row `row`, if that is the code it shows. The row, its
    /// other blocks and their selection stay.
    fn draw_in_row(&self, row: usize, block: usize, done: &Coloured) {
        let shows_it = self
            .shown
            .get(row)
            .and_then(|entry| entry.blocks.get(block))
            .is_some_and(|md| draws(md, &done.job, &self.drawing));
        if !shows_it {
            return;
        }
        let Some(entry) = self.rows().row_data(row) else {
            return;
        };
        let Some(blocks) = entry
            .blocks
            .as_any()
            .downcast_ref::<VecModel<slint_ui::MdBlock>>()
        else {
            return;
        };
        if let Some(mut md) = blocks.row_data(block) {
            md.body = done.text.clone();
            blocks.set_row_data(block, md);
        }
    }

    /// Brings row `index` in line with the changed entry and sends its changed code to be
    /// coloured. Code that only grew keeps its colours until its new ones come (see
    /// [`keeps_colours`]).
    fn set_entry(&mut self, index: usize, entry: present::Entry) {
        order_changed(&self.drawing, index, self.shown.get(index), &entry);
        let synced = match (self.shown.get(index), self.rows().row_data(index)) {
            (Some(old), Some(shown)) => Some(entry.sync(old, shown, &self.drawing)),
            (None, _) | (Some(_), None) => None,
        };
        if let Some(synced) = synced {
            self.rows().set_row_data(index, synced);
        }
        if let Some(slot) = self.shown.get_mut(index) {
            *slot = entry;
        }
    }

    fn rows(&self) -> &VecModel<slint_ui::Entry> {
        self.entries
            .as_any()
            .downcast_ref()
            .expect("the transcript's model is a VecModel, made in SlintFrontend::new")
    }
}

impl Frontend for SlintFrontend {
    fn nav(&mut self, old: Option<&present::Nav>, nav: &present::Nav) {
        model::apply_nav(&self.window, old, nav, &self.drawing);
    }

    fn session(&mut self, old: Option<&present::Session>, session: &present::Session) {
        model::apply_session(&self.window, old, session, &self.drawing);
    }

    fn composer(&mut self, old: Option<&present::Composer>, composer: &present::Composer) {
        model::apply_composer(&self.window, old, composer, &self.drawing);
    }

    fn sessions(&mut self, old: Option<&present::Sessions>, sessions: &present::Sessions) {
        model::apply_sessions(&self.window, old, sessions, &self.drawing);
    }

    fn prompt(&mut self, old: Option<&present::Prompt>, prompt: &present::Prompt) {
        model::apply_prompt(&self.window, old, prompt, &self.drawing);
        self.drawing.order_side(None, Some(prompt));
        self.prompt = Some(prompt.clone());
    }

    fn peek(&mut self, old: Option<&present::Peek>, peek: &present::Peek) {
        model::apply_peek(&self.window, old, peek, &self.drawing);
    }

    fn changes(&mut self, old: Option<&present::Changes>, changes: &present::Changes) {
        model::apply_changes(&self.window, old, changes, &self.drawing);
    }

    fn side(&mut self, old: Option<&present::Side>, side: &present::Side) {
        model::apply_side(&self.window, old, side, &self.drawing);
        self.drawing.order_side(Some(side), None);
        self.side = Some(side.clone());
    }

    fn preferences(
        &mut self,
        old: Option<&present::Preferences>,
        preferences: &present::Preferences,
    ) {
        model::apply_preferences(&self.window, old, preferences, &self.drawing);
    }

    fn usage(&mut self, old: Option<&present::Usage>, usage: &present::Usage) {
        model::apply_usage(&self.window, old, usage, &self.drawing);
    }

    fn transcript(&mut self, old: Option<&present::Transcript>, transcript: &present::Transcript) {
        model::apply_transcript(&self.window, old, transcript, &self.drawing);
    }

    fn entries(&mut self, change: EntriesChange) {
        match change {
            EntriesChange::Reset(entries) => {
                // Shown plain at once, coloured as the thread gets to it.
                self.drawing.renew();
                let rows: Vec<slint_ui::Entry> = entries
                    .iter()
                    .map(|entry| entry.to_slint(&self.drawing))
                    .collect();
                self.rows().set_vec(rows);
                self.shown = entries;
                self.order_all();
            }
            EntriesChange::Push(entry) => {
                order_changed(&self.drawing, self.shown.len(), None, &entry);
                self.rows().push(entry.to_slint(&self.drawing));
                self.shown.push(entry);
            }
            EntriesChange::Set(index, entry) => self.set_entry(index, entry),
        }
    }

    fn show_image(&mut self, key: ImageKey, shown: Result<&Pixels, String>) {
        if let Ok(Some(image)) = shown.map(slint_image) {
            self.drawing.images.insert(key, image);
        }
    }

    fn keep_images(&mut self, shown: &HashSet<ImageKey>) {
        self.drawing.images.retain(|key, _| shown.contains(key));
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn seat(&mut self, seat: crate::present::Seat) {
        self.desk.ask(seat);
    }

    // The browser demo has no desktop of its own to ask.
    #[cfg(target_arch = "wasm32")]
    fn seat(&mut self, seat: crate::present::Seat) {
        tracing::debug!(?seat, "the browser demo does nothing on the desktop");
    }

    fn command(&mut self, command: Command) {
        let window = &self.window;
        match command {
            Command::Copy(text) => window.invoke_copy_text(text.into()),
            Command::SetDraft(text) => window.global::<slint_ui::Composer>().set_draft(text.into()),
            Command::AppendDraft(text) => {
                let composer = window.global::<slint_ui::Composer>();
                let draft = composer.get_draft();
                let joined = if draft.trim().is_empty() {
                    text
                } else {
                    format!("{}\n\n{text}", draft.trim_end())
                };
                composer.set_draft(joined.into());
            }
            Command::FocusComposer => {
                let composer = window.global::<slint_ui::Composer>();
                composer.set_focus_serial(composer.get_focus_serial().wrapping_add(1));
            }
            Command::FocusSide => {
                let side = window.global::<slint_ui::Side>();
                side.set_focus_wanted(true);
                side.set_focus_serial(side.get_focus_serial().wrapping_add(1));
            }
            Command::ClearSelection => {
                window
                    .global::<slint_ui::TextSelection>()
                    .set_owner(SharedString::default());
            }
            Command::ScrollToEnd => {
                let transcript = window.global::<slint_ui::Transcript>();
                transcript.set_scroll_to_end_serial(
                    transcript.get_scroll_to_end_serial().wrapping_add(1),
                );
            }
            Command::OpenAtEnd => {
                let transcript = window.global::<slint_ui::Transcript>();
                transcript.set_opened_serial(transcript.get_opened_serial().wrapping_add(1));
            }
            Command::ShowTasks => {
                let session = window.global::<slint_ui::Session>();
                session.set_tasks_wanted(session.get_tasks_wanted().wrapping_add(1));
            }
            Command::ProjectsOpened => {
                let sessions = window.global::<slint_ui::Sessions>();
                sessions.set_opened_serial(sessions.get_opened_serial().wrapping_add(1));
            }
            Command::NewProjectCreated => {
                let nav = window.global::<slint_ui::Nav>();
                nav.set_created_serial(nav.get_created_serial().wrapping_add(1));
            }
            Command::FocusPage => {
                let nav = window.global::<slint_ui::Nav>();
                nav.set_page_focus_serial(nav.get_page_focus_serial().wrapping_add(1));
                let focus = window.global::<slint_ui::Focus>();
                focus.set_wanted(slint_ui::FocusArea::Page);
                focus.set_wanted_serial(focus.get_wanted_serial().wrapping_add(1));
                focus.set_pending(true);
            }
            Command::ToggleSwitcher => {
                let nav = window.global::<slint_ui::Nav>();
                nav.set_switcher_serial(nav.get_switcher_serial().wrapping_add(1));
            }
            Command::OpenNewProject => {
                let nav = window.global::<slint_ui::Nav>();
                nav.set_new_project_serial(nav.get_new_project_serial().wrapping_add(1));
            }
            Command::ModelIndex(index) => {
                window.global::<slint_ui::Session>().set_model_index(index);
            }
            Command::ModeIndex(index) => {
                window.global::<slint_ui::Session>().set_mode_index(index);
            }
            Command::Reviewing(reviewing) => {
                window.global::<slint_ui::Prompt>().set_reviewing(reviewing);
            }
            Command::ComposerHeight(height_px) => {
                let composer = window.global::<slint_ui::Composer>();
                match height_px {
                    Some(height_px) => {
                        composer.set_chosen_height(f32::from(height_px));
                        composer.set_size(slint_ui::ComposerSize::Chosen);
                    }
                    None => composer.set_size(slint_ui::ComposerSize::Fit),
                }
            }
            Command::TextSize(pixels) => window.global::<slint_ui::Theme>().set_base_size(pixels),
            Command::Look(look) => window.global::<slint_ui::Theme>().set_look(match look {
                Look::Gruvbox => slint_ui::Look::Gruvbox,
                Look::Plain => slint_ui::Look::Plain,
            }),
            Command::RecolourCode => self.recolour_code(),
            Command::DrawColouredCode => self.draw_coloured(),
            // A window of its own system's, ended and started again, would open it again.
            Command::SessionShown(_) => {}
            Command::Quit => {
                if let Err(error) = slint::quit_event_loop() {
                    tracing::warn!(%error, "the window didn't close");
                }
            }
        }
    }

    fn draft(&self) -> String {
        self.window
            .global::<slint_ui::Composer>()
            .get_draft()
            .into()
    }
}

/// Sets Markdown's inline `code` in `family`, as code blocks are. Slint draws it in the generic
/// monospace family, which an application maps in Slint's font collection (as its maintainers
/// advise, slint-ui/slint#12953); fontconfig's monospace is another font.
fn set_monospace(family: &str) {
    use slint::fontique_012::fontique::GenericFamily;
    let mut collection = slint::fontique_012::shared_collection();
    if let Some(id) = collection.family_id(family) {
        collection.set_generic_families(GenericFamily::Monospace, std::iter::once(id));
    }
}

/// Makes a Slint image of `pixels`; `None` for pixels that don't add up.
fn slint_image(pixels: &Pixels) -> Option<slint::Image> {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(pixels.width_px, pixels.height_px);
    let bytes = buffer.make_mut_bytes();
    if bytes.len() != pixels.rgba.len() {
        return None;
    }
    bytes.copy_from_slice(&pixels.rgba);
    Some(slint::Image::from_rgba8(buffer))
}

/// Makes Slint styled text of inline Markdown, or plain text if Slint's parser refuses it, as
/// it does an HTML tag it doesn't know, such as a stray `<T>` outside backticks.
fn styled(source: &str) -> slint::StyledText {
    let source = crate::markdown::sanitize(source);
    slint::StyledText::from_markdown(&source)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(&source))
}

/// Makes styled text of text shown as it is, such as code or tool output, with tabs as spaces,
/// as Slint's text layout draws no tab stops.
pub(crate) fn plain(text: &str) -> slint::StyledText {
    slint::StyledText::from_plain_text(&crate::markdown::sanitize(text).replace('\t', "    "))
}

#[cfg(test)]
mod tests {
    use super::{Drawing, Place, colourist, present};

    const AT: Place = Place::Entry { row: 0, block: 0 };

    fn drawing() -> (Drawing, colourist::Colourist) {
        let (events, _received) = async_channel::unbounded();
        let colours = crate::generated::SyntaxColours::default();
        let (orders, colourist) = colourist::spawn(&colours, events);
        (Drawing::new(orders), colourist)
    }

    fn coloured(generation: u64, raw: &str) -> colourist::Coloured {
        colourist::Coloured {
            job: colourist::Job {
                generation,
                place: AT,
                lang: "rust".into(),
                raw: raw.into(),
            },
            text: slint::StyledText::from_plain_text(raw),
        }
    }

    fn code(lang: &str, raw: &str) -> present::MdBlock {
        present::MdBlock {
            kind: present::BlockKind::Code,
            lang: lang.into(),
            raw: raw.into(),
            ..present::MdBlock::default()
        }
    }

    #[test]
    fn code_coloured_replaces_what_was_coloured_at_its_place() {
        let (mut drawing, _) = drawing();
        assert!(drawing.take(&coloured(0, "fn")));
        assert!(drawing.take(&coloured(0, "fn f")));
        assert!(drawing.coloured_as("rust", "fn").is_none());
        assert!(drawing.coloured_as("rust", "fn f").is_some());
        assert!(drawing.coloured_as("python", "fn f").is_none());
    }

    #[test]
    fn code_shown_at_another_place_keeps_its_colours() {
        let (mut drawing, _) = drawing();
        let elsewhere = colourist::Coloured {
            job: colourist::Job {
                place: Place::Side {
                    exchange: 0,
                    block: 0,
                },
                ..coloured(0, "fn").job
            },
            ..coloured(0, "fn")
        };
        assert!(drawing.take(&elsewhere));
        assert!(drawing.take(&coloured(0, "fn")));
        assert!(drawing.take(&coloured(0, "fn f")));
        assert!(drawing.coloured_as("rust", "fn").is_some());
        assert!(drawing.coloured_as("rust", "fn f").is_some());
    }

    #[test]
    fn code_coloured_for_rows_gone_is_dropped() {
        let (mut drawing, _) = drawing();
        drawing.renew();
        assert!(!drawing.take(&coloured(0, "fn")));
        assert!(drawing.coloured_as("rust", "fn").is_none());
    }

    #[test]
    fn growing_code_keeps_its_colours() {
        let (mut drawing, _) = drawing();
        let keeps = super::keeps_colours;
        assert!(keeps(&code("rust", "fn"), &code("rust", "fn f"), &drawing));
        assert!(!keeps(
            &code("rust", "fn"),
            &code("python", "fn f"),
            &drawing
        ));
        assert!(!keeps(&code("rust", "fn"), &code("rust", "let"), &drawing));
        assert!(drawing.take(&coloured(0, "fn f")));
        assert!(!keeps(&code("rust", "fn"), &code("rust", "fn f"), &drawing));
    }

    #[test]
    fn colours_for_code_since_grown_are_drawn_until_its_own_come() {
        let (mut drawing, _) = drawing();
        let older = coloured(0, "fn f");
        assert!(drawing.take(&older));
        let draws =
            |shown: &present::MdBlock, drawing: &Drawing| super::draws(shown, &older.job, drawing);
        assert!(draws(&code("rust", "fn f"), &drawing));
        assert!(draws(&code("rust", "fn f() {}"), &drawing));
        assert!(!draws(&code("rust", "fn"), &drawing));
        assert!(!draws(&code("python", "fn f() {}"), &drawing));
        assert!(drawing.take(&coloured(0, "fn f() {}")));
        assert!(!draws(&code("rust", "fn f() {}"), &drawing));
    }

    #[test]
    fn growing_code_of_no_language_is_drawn_as_it_grows() {
        use super::ToSlint as _;
        let (drawing, _) = drawing();
        let (old, new) = (code("", "fn"), code("", "fn f"));
        assert!(!super::keeps_colours(&old, &new, &drawing));
        let synced = new.sync(&old, old.to_slint(&drawing), &drawing);
        assert_eq!(
            format!("{:?}", synced.body),
            format!("{:?}", super::plain("fn f"))
        );
    }

    #[test]
    fn markdown_accepted_as_slint_accepts_it() {
        let samples = [
            "plain words",
            "*emphasis* and **strong** and ~~struck~~",
            "a [link](https://example.com) and `code`",
            "- one\n- two\n  - nested",
            "1. first\n2. second",
            "<u>under</u> and <font color=\"red\">red</font>",
            "A stray <T> here.",
            "===",
            "\n\n\\\nAfter a break",
            "# Heading",
            "> quoted",
            "```\ncode\n```",
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "---",
            "![image](x.png)",
            "<div>block</div>",
            "line one\nline two  \nhard break",
            "*unclosed emphasis",
            "an email <me@example.com>",
            "<https://example.com>",
        ];
        for sample in samples {
            let ours = crate::markdown::accepted(sample);
            let slints =
                slint::StyledText::from_markdown(&crate::markdown::sanitize(sample)).is_ok();
            assert_eq!(ours, slints, "{sample:?}");
        }
    }
}
