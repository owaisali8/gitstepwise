//! The credentials panel: find a stored password, API key or login and copy
//! it, or add, change and remove credentials. It opens over the window, and
//! it's the same in every project. The vault itself is in `vault.rs`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::base::actions::Confirm;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zeroize::Zeroizing;

use crate::projects;
use crate::vault::{self, Credential, NewKey, Vault, VaultError};

const KEY_CONTEXT: &str = "Credentials";
/// How long the vault stays unlocked without being used.
const LOCK_AFTER: Duration = Duration::from_secs(15 * 60);
/// How long a copied password stays on the clipboard.
const CLEAR_AFTER: Duration = Duration::from_secs(30);
const MIN_PASSWORD_LEN: usize = 8;
/// What a hidden secret shows: always the same, so it doesn't give away
/// the length.
const HIDDEN: &str = "••••••••••••";
const LABEL_WIDTH: Pixels = px(120.);

gpui_kit::actions!(
    credentials,
    [
        /// Close the panel, or leave a form for the credential it was on.
        Close
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Close, Some(KEY_CONTEXT))]);
}

/// What the workspace needs to hear about.
pub enum CredentialsEvent {
    Close,
    /// Something to say in the status bar, as the panel may have closed.
    Notice(SharedString),
}

/// The list's rows: the credentials that match what's typed, without their
/// secrets.
#[derive(Default)]
struct CredentialList {
    credentials: Vec<Credential>,
    query: String,
    matches: Vec<usize>,
    selected: Option<usize>,
}

impl CredentialList {
    fn set_credentials(&mut self, credentials: Vec<Credential>) {
        self.credentials = credentials;
        self.matches = vault::search(&self.credentials, &self.query);
    }

    fn id_at(&self, row: usize) -> Option<u64> {
        self.matches.get(row).map(|&ix| self.credentials[ix].id)
    }

    fn row_of(&self, id: u64) -> Option<usize> {
        self.matches.iter().position(|&ix| self.credentials[ix].id == id)
    }
}

impl ListDelegate for CredentialList {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.query = query.to_string();
        self.matches = vault::search(&self.credentials, query);
        Task::ready(())
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.matches.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<ListItem> {
        let credential = &self.credentials[*self.matches.get(ix.row)?];
        let detail = [credential.username.as_str(), credential.url.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        let muted = cx.theme().muted_foreground;
        Some(
            ListItem::new(("credential", ix.row))
                .selected(self.selected == Some(ix.row))
                .accessibility_label(credential.name.clone())
                .child(
                    // A set width, so long names are cut short (the list is
                    // 300 wide).
                    v_flex()
                        .w(px(264.))
                        .overflow_hidden()
                        .child(div().text_sm().truncate().child(credential.name.clone()))
                        .when(!detail.is_empty(), |column| {
                            column.child(div().text_xs().text_color(muted).truncate().child(detail))
                        }),
                ),
        )
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<ListState<Self>>) -> impl IntoElement {
        let message = if self.credentials.is_empty() {
            "No credentials yet."
        } else {
            "Nothing matches."
        };
        div()
            .p_4()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(message)
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix.map(|ix| ix.row);
    }
}

enum Screen {
    /// There's no vault yet: choose a master password.
    Create,
    Locked,
    Open(Vault),
}

/// What the right side of an open vault shows.
#[derive(Clone, Copy, PartialEq)]
enum Pane {
    /// The selected credential.
    View,
    /// A new credential (`None`), or changes to one.
    Edit(Option<u64>),
    /// Asking before deleting one.
    Delete(u64),
    ChangePassword,
}

#[derive(Clone, Copy)]
enum Field {
    Username,
    Secret,
    Url,
}

/// The fields of a credential being added or changed.
struct Form {
    name: Entity<InputState>,
    username: Entity<InputState>,
    secret: Entity<InputState>,
    url: Entity<InputState>,
    notes: Entity<TextareaState>,
}

pub struct CredentialsPanel {
    /// Where the vault is kept; `None` without a config folder.
    file: Option<PathBuf>,
    screen: Screen,
    pane: Pane,
    /// Making a key from a password, which takes a moment.
    busy: bool,
    error: Option<SharedString>,
    /// What just happened, at the foot of the panel.
    notice: Option<SharedString>,
    /// Whether the selected credential's secret shows.
    revealed: bool,
    /// The master password, and the same again when choosing one.
    password: Entity<InputState>,
    repeat: Entity<InputState>,
    list: Entity<ListState<CredentialList>>,
    form: Form,
    focus_handle: FocusHandle,
    /// The secret last copied, to take off the clipboard if it's still there.
    copied: Option<Zeroizing<String>>,
    _lock_task: Option<Task<()>>,
    _clear_task: Option<Task<()>>,
    _busy_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CredentialsEvent> for CredentialsPanel {}

impl Focusable for CredentialsPanel {
    /// Where the cursor goes when the panel shows.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match (&self.screen, self.pane) {
            (Screen::Create | Screen::Locked, _) | (Screen::Open(_), Pane::ChangePassword) => {
                self.password.focus_handle(cx)
            }
            (Screen::Open(_), Pane::Edit(_)) => self.form.name.focus_handle(cx),
            (Screen::Open(_), _) => self.list.read(cx).focus_handle(cx),
        }
    }
}

impl CredentialsPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let file = Vault::default_file();
        let screen = if file.as_deref().is_some_and(Path::exists) {
            Screen::Locked
        } else {
            Screen::Create
        };
        let input = |placeholder: &'static str, masked: bool, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).masked(masked).placeholder(placeholder))
        };
        let password = input("Master password", true, window, cx);
        let repeat = input("Repeat the master password", true, window, cx);
        let form = Form {
            name: input("Prod database, OpenAI API key…", false, window, cx),
            username: input("Optional", false, window, cx),
            secret: input("The password, API key or token", true, window, cx),
            url: input("Optional: where it's used", false, window, cx),
            notes: cx.new(|cx| {
                TextareaState::new(window, cx).placeholder("Anything else: ports, recovery codes, who to ask…")
            }),
        };
        let list = cx.new(|cx| ListState::new(CredentialList::default(), window, cx).searchable(true));

        let mut subscriptions = vec![
            cx.subscribe_in(&list, window, Self::on_list_event),
            // A copied password doesn't outlive the app on the clipboard.
            cx.on_app_quit(|this, cx| {
                this.clear_clipboard(cx);
                async {}
            }),
        ];
        for master in [&password, &repeat] {
            subscriptions.push(cx.subscribe_in(master, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit_password(window, cx);
                }
            }));
        }
        for field in [&form.name, &form.username, &form.secret, &form.url] {
            subscriptions.push(cx.subscribe_in(field, window, |this, _, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::PressEnter { .. } => this.save(window, cx),
                    InputEvent::Change => this.touch(window, cx),
                    _ => {}
                }
            }));
        }
        subscriptions.push(cx.subscribe_in(&form.notes, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::Change = event {
                this.touch(window, cx);
            }
        }));

        Self {
            file,
            screen,
            pane: Pane::View,
            busy: false,
            error: None,
            notice: None,
            revealed: false,
            password,
            repeat,
            list,
            form,
            focus_handle: cx.focus_handle(),
            copied: None,
            _lock_task: None,
            _clear_task: None,
            _busy_task: None,
            _subscriptions: subscriptions,
        }
    }

    /// The panel is showing: put the cursor where it's wanted, with the
    /// search box empty.
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notice = None;
        self.revealed = false;
        if self.is_open() && self.pane == Pane::View {
            let selected = self.selected_id(cx);
            self.list.update(cx, |list, cx| list.set_query("", window, cx));
            self.refresh_list(selected, window, cx);
        }
        self.focus_handle(cx).focus(window, cx);
        self.touch(window, cx);
        cx.notify();
    }

    /// Whether the cursor is somewhere in the panel.
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    pub fn is_open(&self) -> bool {
        matches!(self.screen, Screen::Open(_))
    }

    fn vault(&self) -> Option<&Vault> {
        match &self.screen {
            Screen::Open(vault) => Some(vault),
            _ => None,
        }
    }

    fn vault_mut(&mut self) -> Option<&mut Vault> {
        match &mut self.screen {
            Screen::Open(vault) => Some(vault),
            _ => None,
        }
    }

    /// The credential selected in the list.
    fn selected_id(&self, cx: &App) -> Option<u64> {
        let list = self.list.read(cx);
        list.selected_index().and_then(|ix| list.delegate().id_at(ix.row))
    }

    // ---- Locking and unlocking ---------------------------------------------

    /// Enter in a master password field, or the button beside it.
    fn submit_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(file) = self.file.clone() else {
            self.fail("There's no config folder to keep the vault in.", cx);
            return;
        };
        let password = Zeroizing::new(self.password.read(cx).value().to_string());
        let choosing = match (&self.screen, self.pane) {
            (Screen::Create, _) | (Screen::Open(_), Pane::ChangePassword) => true,
            (Screen::Locked, _) => false,
            (Screen::Open(_), _) => return,
        };
        if !choosing {
            if !password.is_empty() {
                let task = cx.background_spawn(async move { Vault::unlock(file, &password) });
                self.wait_for_vault(task, window, cx);
            }
            return;
        }

        if password.chars().count() < MIN_PASSWORD_LEN {
            self.fail(format!("Use at least {MIN_PASSWORD_LEN} characters."), cx);
            self.password.focus_handle(cx).focus(window, cx);
            return;
        }
        let repeat = Zeroizing::new(self.repeat.read(cx).value().to_string());
        if repeat.is_empty() {
            // Enter in the first field moves on to the second.
            self.error = None;
            self.repeat.focus_handle(cx).focus(window, cx);
            cx.notify();
            return;
        }
        if *password != *repeat {
            self.fail("The two passwords aren't the same.", cx);
            self.repeat.update(cx, |input, cx| input.set_value("", window, cx));
            self.repeat.focus_handle(cx).focus(window, cx);
            return;
        }

        match &self.screen {
            Screen::Open(vault) => {
                let cost = vault.cost();
                let task = cx.background_spawn(async move { NewKey::make(&password, cost) });
                self.busy = true;
                self.error = None;
                self._busy_task = Some(cx.spawn_in(window, async move |this, cx| {
                    let key = task.await;
                    this.update_in(cx, |this, window, cx| {
                        this.busy = false;
                        let result = key.and_then(|key| match this.vault_mut() {
                            Some(vault) => vault.change_key(key),
                            None => Ok(()),
                        });
                        match result {
                            Ok(()) => {
                                this.back(window, cx);
                                this.say("Changed the master password.", cx);
                            }
                            Err(err) => this.fail(format!("Couldn't change the master password: {err}"), cx),
                        }
                    })
                    .ok();
                }));
                cx.notify();
            }
            _ => {
                let task = cx.background_spawn(async move { Vault::create(file, &password) });
                self.wait_for_vault(task, window, cx);
            }
        }
    }

    /// Show the vault once `task` has opened or made it.
    fn wait_for_vault(
        &mut self,
        task: Task<Result<Vault, VaultError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        self.error = None;
        self._busy_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(vault) => this.opened(vault, window, cx),
                    Err(err) => {
                        this.fail(err.to_string(), cx);
                        this.password.update(cx, |input, cx| input.select_all(window, cx));
                        this.password.focus_handle(cx).focus(window, cx);
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn opened(&mut self, vault: Vault, window: &mut Window, cx: &mut Context<Self>) {
        for input in [&self.password, &self.repeat] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.screen = Screen::Open(vault);
        self.pane = Pane::View;
        self.revealed = false;
        self.error = None;
        self.refresh_list(None, window, cx);
        if self.contains_focus(window, cx) {
            self.focus_handle(cx).focus(window, cx);
        }
        self.touch(window, cx);
        cx.notify();
    }

    /// Lock the vault: the key and the credentials are dropped, and wiped.
    pub fn lock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_open() {
            return;
        }
        let had_focus = self.contains_focus(window, cx);
        self.screen = Screen::Locked;
        self.pane = Pane::View;
        self.revealed = false;
        self.error = None;
        self.notice = None;
        self._lock_task = None;
        self.list.update(cx, |list, cx| {
            list.delegate_mut().set_credentials(Vec::new());
            list.set_query("", window, cx);
            list.set_selected_index(None, window, cx);
        });
        self.clear_form(window, cx);
        if had_focus {
            self.password.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// The vault was used: it locks `LOCK_AFTER` from now.
    fn touch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_open() {
            return;
        }
        self._lock_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(LOCK_AFTER).await;
            this.update_in(cx, |this, window, cx| {
                this.lock(window, cx);
                this.say("Locked the credentials after 15 minutes without use.", cx);
            })
            .ok();
        }));
    }

    // ---- The list ----------------------------------------------------------

    /// Show the vault's credentials in the list, selecting `select` (or else
    /// the first).
    fn refresh_list(&mut self, select: Option<u64>, window: &mut Window, cx: &mut Context<Self>) {
        let credentials: Vec<Credential> = self
            .vault()
            .map(|vault| vault.credentials().iter().map(Credential::without_secret).collect())
            .unwrap_or_default();
        self.list.update(cx, |list, cx| {
            list.delegate_mut().set_credentials(credentials);
            // A credential just saved is shown even if it doesn't match
            // what was typed.
            let hidden = select.is_some_and(|id| list.delegate().row_of(id).is_none());
            if hidden {
                list.set_query("", window, cx);
            }
            let row = select
                .and_then(|id| list.delegate().row_of(id))
                .or((!list.delegate().matches.is_empty()).then_some(0));
            list.set_selected_index(row.map(IndexPath::new), window, cx);
            if let Some(row) = row {
                list.scroll_to_item(IndexPath::new(row), ScrollStrategy::Center, window, cx);
            }
            cx.notify();
        });
    }

    fn on_list_event(
        &mut self,
        _: &Entity<ListState<CredentialList>>,
        event: &ListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ListEvent::Select(_) | ListEvent::Confirm(_) => {
                if self.pane == Pane::View {
                    self.revealed = false;
                    self.notice = None;
                    self.error = None;
                }
                self.touch(window, cx);
                cx.notify();
            }
            // Escape closes the panel through `Close`.
            ListEvent::Cancel => {}
        }
    }

    /// Enter in the list copies the selected credential's password and
    /// closes the panel; Ctrl+Enter copies the username. (A click on a row
    /// confirms it too, but not through this action, so it only selects.)
    fn on_confirm(&mut self, confirm: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane != Pane::View {
            return;
        }
        if let Some(id) = self.selected_id(cx) {
            let field = if confirm.secondary {
                Field::Username
            } else {
                Field::Secret
            };
            self.copy(id, field, true, window, cx);
        }
    }

    // ---- Copying -----------------------------------------------------------

    /// Copy one of a credential's fields. A copied secret is taken off the
    /// clipboard after `CLEAR_AFTER`, if it's still there.
    fn copy(&mut self, id: u64, field: Field, then_close: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(credential) = self.vault().and_then(|vault| vault.get(id)) else {
            return;
        };
        let name = credential.name.clone();
        let (value, message) = match field {
            Field::Username => (&credential.username, format!("Copied the username for “{name}”.")),
            Field::Url => (&credential.url, format!("Copied the address for “{name}”.")),
            Field::Secret => (
                &credential.secret,
                format!(
                    "Copied the password for “{name}”. It's cleared from the clipboard in {} seconds.",
                    CLEAR_AFTER.as_secs()
                ),
            ),
        };
        if value.is_empty() {
            let what = match field {
                Field::Username => "username",
                Field::Url => "host or URL",
                Field::Secret => "password or key",
            };
            self.say(format!("“{name}” has no {what}."), cx);
            return;
        }
        let value = Zeroizing::new(value.clone());
        cx.write_to_clipboard(ClipboardItem::new_string(value.to_string()));
        if let Field::Secret = field {
            self.copied = Some(value);
            self._clear_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(CLEAR_AFTER).await;
                this.update(cx, |this, cx| {
                    if this.clear_clipboard(cx) {
                        this.say("Cleared the copied password from the clipboard.", cx);
                    }
                })
                .ok();
            }));
        }
        self.touch(window, cx);
        self.say(message, cx);
        if then_close {
            cx.emit(CredentialsEvent::Close);
        }
    }

    /// Take the copied secret off the clipboard, if it's still what's
    /// there. Returns whether it was.
    fn clear_clipboard(&mut self, cx: &mut App) -> bool {
        let Some(copied) = self.copied.take() else {
            return false;
        };
        let still_there = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .map(Zeroizing::new)
            .is_some_and(|text| *text == *copied);
        if still_there {
            cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
        }
        still_there
    }

    fn say(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let message = message.into();
        self.notice = Some(message.clone());
        cx.emit(CredentialsEvent::Notice(message));
        cx.notify();
    }

    fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        cx.notify();
    }

    // ---- Adding, changing and deleting ---------------------------------------

    fn start_edit(&mut self, id: Option<u64>, window: &mut Window, cx: &mut Context<Self>) {
        let credential = id.and_then(|id| self.vault()?.get(id).cloned()).unwrap_or_default();
        let fields = [
            (&self.form.name, &credential.name),
            (&self.form.username, &credential.username),
            (&self.form.secret, &credential.secret),
            (&self.form.url, &credential.url),
        ];
        for (input, value) in fields {
            input.update(cx, |input, cx| input.set_value(value.clone(), window, cx));
        }
        // The secret starts hidden, even if it was shown last time.
        self.form.secret.update(cx, |input, cx| input.set_masked(true, window, cx));
        self.form
            .notes
            .update(cx, |notes, cx| notes.set_value(credential.notes.clone(), window, cx));
        self.pane = Pane::Edit(id);
        self.error = None;
        self.notice = None;
        self.form.name.focus_handle(cx).focus(window, cx);
        self.touch(window, cx);
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Pane::Edit(id) = self.pane else {
            return;
        };
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let name = text(&self.form.name).trim().to_string();
        if name.is_empty() {
            self.fail("Give it a name, to find it by.", cx);
            self.form.name.focus_handle(cx).focus(window, cx);
            return;
        }
        let credential = Credential {
            id: id.unwrap_or(0),
            name,
            username: text(&self.form.username).trim().to_string(),
            // As typed: a password can start or end with a space.
            secret: text(&self.form.secret),
            url: text(&self.form.url).trim().to_string(),
            notes: self.form.notes.read(cx).value().trim_end().to_string(),
            updated: 0,
        };
        let Some(vault) = self.vault_mut() else {
            return;
        };
        let previous = id.and_then(|id| vault.get(id).cloned());
        let id = vault.put(credential, projects::now_ms());
        if let Err(err) = vault.save() {
            // Keep what's in memory the same as what's on disk.
            match previous {
                Some(previous) => {
                    let updated = previous.updated;
                    vault.put(previous, updated);
                }
                None => drop(vault.remove(id)),
            }
            self.fail(format!("Couldn't save the vault: {err}"), cx);
            return;
        }
        self.back(window, cx);
        self.refresh_list(Some(id), window, cx);
        self.touch(window, cx);
    }

    fn delete(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        // Select the next one down after, or else the one above.
        let next = {
            let list = self.list.read(cx).delegate();
            list.row_of(id)
                .and_then(|row| list.id_at(row + 1).or_else(|| list.id_at(row.checked_sub(1)?)))
        };
        let Some(vault) = self.vault_mut() else {
            return;
        };
        let Some(removed) = vault.remove(id) else {
            return;
        };
        if let Err(err) = vault.save() {
            let updated = removed.updated;
            vault.put(removed, updated);
            self.pane = Pane::View;
            self.fail(format!("Couldn't save the vault: {err}"), cx);
            return;
        }
        let name = removed.name.clone();
        self.back(window, cx);
        self.refresh_list(next, window, cx);
        self.say(format!("Deleted “{name}”."), cx);
    }

    /// Leave a form or question for the credential selected.
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_form(window, cx);
        self.pane = Pane::View;
        self.error = None;
        if self.contains_focus(window, cx) {
            self.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// Empty the form and the password fields, so no secret is left in them.
    fn clear_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [
            &self.form.name,
            &self.form.username,
            &self.form.secret,
            &self.form.url,
            &self.password,
            &self.repeat,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.form.notes.update(cx, |notes, cx| notes.set_value("", window, cx));
    }

    fn close(&mut self, _: &Close, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_open() && self.pane != Pane::View {
            self.back(window, cx);
        } else {
            cx.emit(CredentialsEvent::Close);
        }
    }

    // ---- Drawing -------------------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let count = self.vault().map(|vault| vault.credentials().len());
        h_flex()
            .h(px(40.))
            .flex_none()
            .items_center()
            .gap_2()
            .pl_4()
            .pr_2()
            .border_b_1()
            .border_color(theme.border)
            .child(Icon::new(LucideIcon::KeyRound).small().text_color(theme.muted_foreground))
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Credentials"))
            .when_some(count, |row, count| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{count} saved")),
                )
            })
            .child(div().flex_1())
            .when(self.is_open(), |row| {
                row.child(
                    Button::new("change-master-password")
                        .ghost()
                        .xsmall()
                        .label("Change Master Password…")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.back(window, cx);
                            this.pane = Pane::ChangePassword;
                            this.password.focus_handle(cx).focus(window, cx);
                        })),
                )
                .child(
                    Button::new("lock-credentials")
                        .ghost()
                        .xsmall()
                        .icon(LucideIcon::Lock)
                        .label("Lock")
                        .tooltip("Lock the credentials. The master password opens them again.")
                        .on_click(cx.listener(|this, _, window, cx| this.lock(window, cx))),
                )
            })
            .child(
                Button::new("close-credentials")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Close (Esc)")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(CredentialsEvent::Close))),
            )
    }

    /// Choosing the master password, or typing it to unlock.
    fn render_gate(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let creating = matches!(self.screen, Screen::Create);
        let (icon, title, intro, action) = if creating {
            (
                LucideIcon::KeyRound,
                "Keep your secrets in one place",
                "API keys, passwords, server logins: keep them here instead of in a text file. \
                 They're encrypted with a master password, and they're the same in every project.",
                "Create Vault",
            )
        } else {
            (
                LucideIcon::Lock,
                "The credentials are locked",
                "Type the master password to open them.",
                "Unlock",
            )
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .px_8()
            .child(Icon::new(icon).large().text_color(muted))
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(div().max_w(px(440.)).text_sm().text_center().text_color(muted).child(intro))
            .child(
                v_flex()
                    .w(px(320.))
                    .gap_2()
                    .mt_2()
                    .child(Input::new(&self.password).mask_toggle())
                    .when(creating, |column| column.child(Input::new(&self.repeat).mask_toggle()))
                    .child(
                        Button::new("submit-master-password")
                            .primary()
                            .w_full()
                            .label(action)
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.submit_password(window, cx))),
                    )
                    .when_some(self.error.clone(), |column, error| {
                        column.child(div().text_sm().text_color(danger).child(error))
                    }),
            )
            .when(creating, |column| {
                column.child(
                    div()
                        .max_w(px(440.))
                        .text_xs()
                        .text_center()
                        .text_color(muted)
                        .child(format!(
                            "Use at least {MIN_PASSWORD_LEN} characters. If you forget the master \
                             password, there's no way to get the credentials back."
                        )),
                )
            })
    }

    /// A label and what goes with it, in a row.
    fn row(label: &'static str, muted: Hsla, content: impl IntoElement) -> Div {
        h_flex()
            .items_center()
            .gap_2()
            .min_h(px(28.))
            .child(div().w(LABEL_WIDTH).flex_none().text_sm().text_color(muted).child(label))
            .child(content)
    }

    fn render_credential(&self, credential: &Credential, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, mono) = (theme.muted_foreground, theme.mono_font_family.clone());
        let id = credential.id;
        let copy = |field: Field, ix: usize| {
            Button::new(("copy-field", ix))
                .ghost()
                .xsmall()
                .icon(IconName::Copy)
                .tooltip("Copy")
                .on_click(cx.listener(move |this, _, window, cx| this.copy(id, field, false, window, cx)))
        };
        let value = |text: String| {
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .font_family(mono.clone())
                .truncate()
                .child(text)
        };
        let secret = if self.revealed {
            // Shown in full, a long key wraps.
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .font_family(mono.clone())
                .child(credential.secret.clone())
        } else {
            value(HIDDEN.into())
        };

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .mb_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(credential.name.clone()),
                    )
                    .child(
                        Button::new("edit-credential")
                            .ghost()
                            .small()
                            .icon(LucideIcon::Pencil)
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| this.start_edit(Some(id), window, cx))),
                    )
                    .child(
                        Button::new("delete-credential")
                            .ghost()
                            .small()
                            .icon(LucideIcon::Trash)
                            .label("Delete")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pane = Pane::Delete(id);
                                cx.notify();
                            })),
                    ),
            )
            .when(!credential.username.is_empty(), |column| {
                column.child(
                    Self::row("Username", muted, value(credential.username.clone())).child(copy(Field::Username, 0)),
                )
            })
            .when(!credential.secret.is_empty(), |column| {
                column.child(
                    Self::row("Password or key", muted, secret)
                        .child(
                            Button::new("reveal-secret")
                                .ghost()
                                .xsmall()
                                .icon(if self.revealed { IconName::EyeOff } else { IconName::Eye })
                                .tooltip(if self.revealed { "Hide" } else { "Show" })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.revealed = !this.revealed;
                                    this.touch(window, cx);
                                    cx.notify();
                                })),
                        )
                        .child(copy(Field::Secret, 1)),
                )
            })
            .when(!credential.url.is_empty(), |column| {
                column.child(Self::row("Host or URL", muted, value(credential.url.clone())).child(copy(Field::Url, 2)))
            })
            .when(!credential.notes.is_empty(), |column| {
                column.child(
                    h_flex()
                        .items_start()
                        .gap_2()
                        .pt_1()
                        .child(div().w(LABEL_WIDTH).flex_none().text_sm().text_color(muted).child("Notes"))
                        .child(div().flex_1().min_w_0().text_sm().child(credential.notes.clone())),
                )
            })
    }

    fn render_form(&self, id: Option<u64>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let field = |input: &Entity<InputState>| div().flex_1().min_w_0().child(Input::new(input).small());
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .mb_1()
                    .child(if id.is_some() { "Edit credential" } else { "New credential" }),
            )
            .child(Self::row("Name", muted, field(&self.form.name)))
            .child(Self::row("Username", muted, field(&self.form.username)))
            .child(Self::row(
                "Password or key",
                muted,
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.form.secret).small().mask_toggle()),
            ))
            .child(Self::row("Host or URL", muted, field(&self.form.url)))
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(div().w(LABEL_WIDTH).flex_none().pt_1().text_sm().text_color(muted).child("Notes"))
                    .child(div().flex_1().min_w_0().h(px(96.)).child(Textarea::new(&self.form.notes).h_full())),
            )
            .when_some(self.error.clone(), |column, error| {
                column.child(div().text_sm().text_color(danger).child(error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .mt_1()
                    .child(
                        Button::new("cancel-edit")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                    )
                    .child(
                        Button::new("save-credential")
                            .primary()
                            .small()
                            .label("Save")
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }

    fn render_delete(&self, credential: &Credential, cx: &mut Context<Self>) -> impl IntoElement {
        let id = credential.id;
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Delete “{}”?", credential.name)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("It's taken out of the vault for good."),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("confirm-delete")
                            .danger()
                            .small()
                            .label("Delete")
                            .on_click(cx.listener(move |this, _, window, cx| this.delete(id, window, cx))),
                    )
                    .child(
                        Button::new("cancel-delete")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                    ),
            )
    }

    fn render_change_password(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let field = |input: &Entity<InputState>| {
            div().flex_1().min_w_0().child(Input::new(input).small().mask_toggle())
        };
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Change the master password"),
            )
            .child(div().text_sm().text_color(muted).mb_1().child(format!(
                "The vault is encrypted again with the new password. Use at least {MIN_PASSWORD_LEN} \
                 characters. If you forget it, there's no way to get the credentials back."
            )))
            .child(Self::row("New password", muted, field(&self.password)))
            .child(Self::row("Repeat it", muted, field(&self.repeat)))
            .when_some(self.error.clone(), |column, error| {
                column.child(div().text_sm().text_color(danger).child(error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .mt_1()
                    .child(
                        Button::new("cancel-change-password")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                    )
                    .child(
                        Button::new("change-password")
                            .primary()
                            .small()
                            .label("Change Password")
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.submit_password(window, cx))),
                    ),
            )
    }

    /// The right side of an open vault.
    fn render_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let Some(vault) = self.vault() else {
            return div().into_any_element();
        };
        let selected = self.selected_id(cx).and_then(|id| vault.get(id));
        match self.pane {
            Pane::Edit(id) => self.render_form(id, cx).into_any_element(),
            Pane::ChangePassword => self.render_change_password(cx).into_any_element(),
            Pane::Delete(id) => match vault.get(id) {
                Some(credential) => self.render_delete(credential, cx).into_any_element(),
                None => div().into_any_element(),
            },
            Pane::View => match selected {
                Some(credential) => self.render_credential(credential, cx).into_any_element(),
                None if vault.credentials().is_empty() => v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("No credentials yet"))
                    .child(
                        div()
                            .max_w(px(320.))
                            .text_sm()
                            .text_center()
                            .text_color(muted)
                            .child("Add API keys, passwords and server logins, then find them here by typing."),
                    )
                    .child(
                        Button::new("first-credential")
                            .primary()
                            .small()
                            .mt_2()
                            .icon(IconName::Plus)
                            .label("New Credential")
                            .on_click(cx.listener(|this, _, window, cx| this.start_edit(None, window, cx))),
                    )
                    .into_any_element(),
                None => div()
                    .text_sm()
                    .text_color(muted)
                    .child("Choose a credential, or add one with New Credential.")
                    .into_any_element(),
            },
        }
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let open = self.is_open();
        let (message, color) = match (&self.error, &self.notice) {
            (Some(error), _) if open && self.pane == Pane::View => (error.clone(), danger),
            (_, Some(notice)) => (notice.clone(), muted),
            _ if open && matches!(self.pane, Pane::Edit(_)) => ("Enter saves · Esc cancels".into(), muted),
            _ if open && self.pane != Pane::View => ("Esc cancels".into(), muted),
            _ if open => (
                format!(
                    "Enter copies the password · {} the username · Esc closes",
                    crate::shortcut("Enter")
                )
                .into(),
                muted,
            ),
            _ => match &self.file {
                Some(file) => (
                    format!("Kept in {}", projects::display_path(file, dirs::home_dir().as_deref())).into(),
                    muted,
                ),
                None => (SharedString::default(), muted),
            },
        };
        h_flex()
            .h(px(32.))
            .flex_none()
            .items_center()
            .gap_3()
            .pl_3()
            .pr_2()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .child(div().flex_1().min_w_0().truncate().text_color(color).child(message))
            .when(open, |row| {
                row.child(
                    Button::new("new-credential")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label("New Credential")
                        .on_click(cx.listener(|this, _, window, cx| this.start_edit(None, window, cx))),
                )
            })
    }
}

impl Render for CredentialsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let body = if self.is_open() {
            // The list is out of reach while a form is open, so a click
            // there can't leave the form half done.
            let in_form = self.pane != Pane::View;
            h_flex()
                .size_full()
                .child(
                    div()
                        .relative()
                        .w(px(300.))
                        .h_full()
                        .flex_none()
                        .border_r_1()
                        .border_color(border)
                        .when(in_form, |column| column.opacity(0.5))
                        .child(List::new(&self.list).search_placeholder("Search credentials…"))
                        .when(in_form, |column| column.child(div().absolute().inset_0().occlude())),
                )
                .child(
                    div()
                        .id("credential-pane")
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_y_scroll()
                        .p_4()
                        .child(self.render_pane(cx)),
                )
                .into_any_element()
        } else {
            self.render_gate(cx).into_any_element()
        };

        v_flex()
            .id("credentials")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close))
            .capture_action(cx.listener(Self::on_confirm))
            .size_full()
            .popover_style(cx)
            .occlude()
            .overflow_hidden()
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(body))
            .child(self.render_footer(cx))
    }
}
