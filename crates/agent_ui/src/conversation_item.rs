//! Hosts a live [`ConversationView`] as a center-pane workspace [`Item`].
//!
//! The agent panel remains the coordinator that creates threads (it owns the
//! `AgentConnectionStore`, `ThreadStore`, and `fs`); this wrapper only renders
//! an already-created view in a pane. A thread is hosted in exactly one place
//! at a time, so the item is never cloned on split.

use std::time::Duration;

use agent_client_protocol::schema::v1 as acp;
use gpui::{
    Animation, AnimationExt as _, App, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, WeakEntity, Window, prelude::*, pulsating_between,
};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{Tab, Tooltip, prelude::*, utils::WithRemSize};
use workspace::{Item, PathList, Workspace, item::ItemEvent, pane};

use crate::agent_panel::AgentPanel;
use crate::conversation_host::{ConversationHost, VisibilityChangedCallback};
use crate::conversation_view::ConversationView;
use crate::thread_metadata_store::ThreadId;
use crate::{Agent, MoveThreadToPanel};

pub struct ConversationItem {
    view: Entity<ConversationView>,
    /// The hosted view's workspace, captured here so host queries (which can
    /// run while the view itself is being updated) never read the view.
    workspace: WeakEntity<Workspace>,
    _subscriptions: Vec<Subscription>,
}

impl ConversationItem {
    pub fn new(view: Entity<ConversationView>, cx: &mut Context<Self>) -> Self {
        let _subscriptions = vec![cx.observe(&view, |_this, _view, cx| {
            // Keep the tab title in sync with the thread title and re-render
            // our chrome whenever the hosted view changes.
            cx.emit(());
            cx.notify();
        })];

        let workspace = view.read(cx).workspace().clone();

        Self {
            view,
            workspace,
            _subscriptions,
        }
    }

    pub fn conversation_view(&self) -> &Entity<ConversationView> {
        &self.view
    }

    /// Hosts `view` as a center-pane item, or activates the item already
    /// hosting the same thread — a thread is hosted in exactly one place, so
    /// the workspace never holds two items for one `ThreadId`.
    pub fn deploy(
        view: Entity<ConversationView>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let thread_id = view.read(cx).thread_id;
        if let Some(existing) = Self::find_for_thread(workspace, thread_id, cx) {
            workspace.activate_item(&existing, true, true, window, cx);
            existing
        } else {
            let item = cx.new(|cx| Self::new(view, cx));
            workspace.add_item_to_center(Box::new(item.clone()), window, cx);
            item
        }
    }

    /// The center-pane item hosting `thread_id` in `workspace`, if any.
    pub fn find_for_thread(
        workspace: &Workspace,
        thread_id: ThreadId,
        cx: &App,
    ) -> Option<Entity<Self>> {
        workspace
            .items_of_type::<Self>(cx)
            .find(|item| item.read(cx).view.read(cx).thread_id == thread_id)
    }

    /// The center-pane item hosting the thread for `session_id`, if any.
    pub(crate) fn find_for_session(
        workspace: &Workspace,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<Entity<Self>> {
        workspace
            .items_of_type::<Self>(cx)
            .find(|item| item.read(cx).view.read(cx).root_session_id.as_ref() == Some(session_id))
    }

    /// Activates the center-pane item hosting `thread_id`, if one exists.
    /// Returns whether an item was found; callers fall back to the
    /// agent-panel path when this returns `false`.
    pub fn activate_for_thread(
        workspace: &mut Workspace,
        thread_id: ThreadId,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(item) = Self::find_for_thread(workspace, thread_id, cx) else {
            return false;
        };
        workspace.activate_item(&item, true, focus, window, cx);
        true
    }

    /// Transfers the agent panel's visible thread into a center-pane item
    /// (the [`OpenThreadInCenter`](crate::OpenThreadInCenter) transfer).
    /// When the thread already lives in the center — which the uniqueness
    /// invariant should prevent — the existing item is activated instead.
    /// Thread creation stays panel-mediated: the item only ever receives a
    /// live view the panel built.
    pub(crate) fn open_visible_panel_thread_in_center(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Self>> {
        let panel = workspace.panel::<AgentPanel>(cx)?;
        let thread_id = panel.read(cx).active_thread_id(cx)?;
        if let Some(existing) = Self::find_for_thread(workspace, thread_id, cx) {
            workspace.activate_item(&existing, true, true, window, cx);
            return Some(existing);
        }
        let view = panel.update(cx, |panel, cx| {
            panel.take_visible_thread_for_center(window, cx)
        })?;
        Some(Self::deploy(view, workspace, window, cx))
    }

    /// Transfers the hosted thread back into the agent panel (the
    /// [`MoveThreadToPanel`] transfer): closes this item and hands the live
    /// view to the panel, which reveals and focuses it.
    fn move_to_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let view = self.view.clone();
        let item = cx.entity();

        // Removing the item from its pane invokes `Item` callbacks on it
        // (`deactivated`, `on_removed`), so the transfer runs deferred,
        // outside this entity's update.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                    return;
                };
                if let Some(pane) = workspace.pane_for(&item) {
                    pane.update(cx, |pane, cx| {
                        pane.remove_item(item.entity_id(), false, true, window, cx);
                    });
                }
                workspace.reveal_panel::<AgentPanel>(window, cx);
                panel.update(cx, |panel, cx| {
                    panel.adopt_conversation_view(view, true, window, cx);
                });
                workspace.focus_panel::<AgentPanel>(window, cx);
            });
        });
    }

    /// The chrome the agent panel's toolbar owns when the thread lives in the
    /// panel: the (editable) thread title.
    fn render_title(&self, cx: &mut Context<Self>) -> AnyElement {
        let view_ref = self.view.read(cx);

        let is_generating_title = view_ref
            .as_native_thread(cx)
            .is_some_and(|thread| thread.read(cx).is_generating_title());

        let Some(title_editor) = view_ref
            .root_thread_view()
            .map(|thread_view| thread_view.read(cx).title_editor.clone())
        else {
            return Label::new(view_ref.title(cx))
                .color(Color::Muted)
                .truncate()
                .into_any_element();
        };

        if is_generating_title {
            Label::new(view_ref.title(cx))
                .color(Color::Muted)
                .truncate()
                .with_animation(
                    "generating_title",
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.4, 0.8)),
                    |label, delta| label.alpha(delta),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .w_full()
                .on_action({
                    let view = self.view.downgrade();
                    move |_: &menu::Confirm, window, cx| {
                        if let Some(view) = view.upgrade() {
                            view.focus_handle(cx).focus(window, cx);
                        }
                    }
                })
                .on_action({
                    let view = self.view.downgrade();
                    move |_: &editor::actions::Cancel, window, cx| {
                        if let Some(view) = view.upgrade() {
                            view.focus_handle(cx).focus(window, cx);
                        }
                    }
                })
                .child(title_editor)
                .into_any_element()
        }
    }
}

impl EventEmitter<()> for ConversationItem {}

impl Focusable for ConversationItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

impl Item for ConversationItem {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.view.read(cx).title(cx)
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ZedAssistant).color(Color::Muted))
    }

    fn to_item_events(_event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(ItemEvent::UpdateTab);
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Agent Thread Opened In Center")
    }

    // `can_split` stays false (the trait default), so `clone_on_split` is
    // never called: a thread is hosted exactly once.
}

/// A center-pane host wraps exactly one conversation: the view is visible
/// when its item is the active item of the pane hosting it.
impl ConversationHost for Entity<ConversationItem> {
    fn is_view_visible(&self, view: &Entity<ConversationView>, cx: &App) -> bool {
        if self.read(cx).view.entity_id() != view.entity_id() {
            return false;
        }
        let Some(workspace) = self.read(cx).workspace.upgrade() else {
            return false;
        };

        workspace.read(cx).pane_for(self).is_some_and(|pane| {
            pane.read(cx)
                .active_item()
                .is_some_and(|active| active.item_id() == self.entity_id())
        })
    }

    fn reveal_thread(
        &self,
        _agent: Agent,
        _thread_id: ThreadId,
        _work_dirs: Option<PathList>,
        _title: Option<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) {
        // An item hosts exactly one conversation, so the thread to reveal is
        // already determined by `self`: activating the item reveals it.
        let Some(workspace) = self.read(cx).workspace.upgrade() else {
            return;
        };

        workspace.update(cx, |workspace, cx| {
            workspace.activate_item(self, true, true, window, cx);
        });
    }

    fn subscribe_to_visibility_changes(
        &self,
        window: &Window,
        cx: &mut Context<ConversationView>,
        on_change: VisibilityChangedCallback,
    ) -> Option<Subscription> {
        let workspace = self.read(cx).workspace.upgrade()?;
        let pane = workspace.read(cx).pane_for(self)?;

        Some(cx.subscribe_in(
            &pane,
            window,
            move |this, _, event: &pane::Event, window, cx| match event {
                pane::Event::ActivateItem { .. } | pane::Event::Focus => {
                    on_change(this, window, cx);
                }
                _ => {}
            },
        ))
    }
}

impl Render for ConversationItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title_strip = h_flex()
            .id("conversation-item-title-strip")
            .h(Tab::container_height(cx))
            .w_full()
            .flex_shrink_0()
            .px(DynamicSpacing::Base08.rems(cx))
            .gap(DynamicSpacing::Base04.rems(cx))
            .bg(cx.theme().colors().tab_bar_background)
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(self.render_title(cx))
            .child(
                IconButton::new("move-thread-to-panel", IconName::ArrowRightLeft)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("Move Thread to Agent Panel", &MoveThreadToPanel, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.move_to_panel(window, cx);
                    })),
            );

        let content = v_flex()
            .size_full()
            .on_action(cx.listener(|this, _: &MoveThreadToPanel, window, cx| {
                this.move_to_panel(window, cx);
            }))
            .child(title_strip)
            .child(self.view.clone());

        WithRemSize::new(ThemeSettings::get_global(cx).agent_ui_font_size(cx))
            .size_full()
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_connection_store::AgentConnectionStore;
    use crate::conversation_view::tests::init_test;
    use crate::test_support::{
        StubAgentServer, active_thread_id, open_thread_with_connection, send_message,
    };
    use crate::{Agent, AgentThreadSource};
    use acp_thread::StubAgentConnection;
    use agent::ThreadStore;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::json;
    use std::path::Path;
    use std::rc::Rc;
    use workspace::{MultiWorkspace, Workspace};

    async fn setup_conversation_view(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ConversationView>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::default_response()),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project,
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();

        (conversation_view, workspace, cx)
    }

    #[gpui::test]
    async fn test_tab_content_text_is_thread_title(cx: &mut TestAppContext) {
        init_test(cx);
        let (conversation_view, _workspace, cx) = setup_conversation_view(cx).await;

        let item = cx.update(|_window, cx| {
            cx.new(|cx| ConversationItem::new(conversation_view.clone(), cx))
        });

        cx.read(|cx| {
            let expected = conversation_view.read(cx).title(cx);
            assert_eq!(item.read(cx).tab_content_text(0, cx), expected);
        });
    }

    #[gpui::test]
    async fn test_focus_delegates_to_view(cx: &mut TestAppContext) {
        init_test(cx);
        let (conversation_view, _workspace, cx) = setup_conversation_view(cx).await;

        let item = cx.update(|_window, cx| {
            cx.new(|cx| ConversationItem::new(conversation_view.clone(), cx))
        });

        cx.read(|cx| {
            assert_eq!(
                item.read(cx).focus_handle(cx),
                conversation_view.read(cx).focus_handle(cx)
            );
        });
    }

    #[gpui::test]
    async fn test_renders_as_center_pane_item(cx: &mut TestAppContext) {
        init_test(cx);
        let (conversation_view, workspace, cx) = setup_conversation_view(cx).await;

        let item = cx.update(|_window, cx| {
            cx.new(|cx| ConversationItem::new(conversation_view.clone(), cx))
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        let active = workspace
            .read_with(cx, |workspace, cx| workspace.active_item(cx))
            .expect("center pane should have an active item");
        let active = active
            .downcast::<ConversationItem>()
            .expect("active item should be a ConversationItem");
        assert_eq!(active.entity_id(), item.entity_id());
        cx.read(|cx| {
            assert_eq!(
                active.read(cx).conversation_view().entity_id(),
                conversation_view.entity_id()
            );
        });
    }

    #[gpui::test]
    async fn test_events_update_tab(cx: &mut TestAppContext) {
        init_test(cx);

        let mut item_events = Vec::new();
        <ConversationItem as Item>::to_item_events(&(), &mut |event| item_events.push(event));
        assert_eq!(item_events, vec![ItemEvent::UpdateTab]);
    }

    #[gpui::test]
    async fn test_host_visibility_tracks_pane_active_item(cx: &mut TestAppContext) {
        init_test(cx);
        let (conversation_view, workspace, cx) = setup_conversation_view(cx).await;

        let item = cx.update(|_window, cx| {
            cx.new(|cx| ConversationItem::new(conversation_view.clone(), cx))
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        cx.read(|cx| {
            assert!(
                item.is_view_visible(&conversation_view, cx),
                "the view should be visible while its item is the active pane item"
            );
        });

        workspace.update_in(cx, |workspace, window, cx| {
            let placeholder = cx.new(|cx| PlaceholderItem {
                focus_handle: cx.focus_handle(),
            });
            workspace.add_item_to_active_pane(Box::new(placeholder), None, true, window, cx);
        });
        cx.run_until_parked();

        cx.read(|cx| {
            assert!(
                !item.is_view_visible(&conversation_view, cx),
                "the view should be hidden while another pane item is active"
            );
        });
    }

    #[gpui::test]
    async fn test_reveal_thread_activates_item(cx: &mut TestAppContext) {
        init_test(cx);
        let (conversation_view, workspace, cx) = setup_conversation_view(cx).await;

        let item = cx.update(|_window, cx| {
            cx.new(|cx| ConversationItem::new(conversation_view.clone(), cx))
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
            let placeholder = cx.new(|cx| PlaceholderItem {
                focus_handle: cx.focus_handle(),
            });
            workspace.add_item_to_active_pane(Box::new(placeholder), None, true, window, cx);
        });
        cx.run_until_parked();

        let thread_id = conversation_view.read_with(cx, |view, _cx| view.thread_id);
        cx.update(|window, cx| {
            item.reveal_thread(
                Agent::Custom { id: "Test".into() },
                thread_id,
                None,
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let active = workspace
            .read_with(cx, |workspace, cx| workspace.active_item(cx))
            .and_then(|active| active.downcast::<ConversationItem>());
        assert_eq!(
            active.map(|active| active.entity_id()),
            Some(item.entity_id()),
            "reveal_thread should re-activate the item hosting the thread"
        );
    }

    /// A workspace whose agent panel is installed in a dock, ready for
    /// panel ⇄ center-pane thread transfers.
    async fn setup_workspace_with_panel(
        cx: &mut TestAppContext,
    ) -> (
        Entity<AgentPanel>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();

        (panel, workspace, cx)
    }

    #[gpui::test]
    async fn test_open_thread_in_center_transfers_panel_thread(cx: &mut TestAppContext) {
        let (panel, workspace, cx) = setup_workspace_with_panel(cx).await;
        open_thread_with_connection(&panel, StubAgentConnection::new(), cx);
        send_message(&panel, cx);

        let view = panel.read_with(cx, |panel, _| {
            panel.active_conversation_view().unwrap().clone()
        });
        let thread_id = active_thread_id(&panel, cx);

        let item = workspace
            .update_in(cx, |workspace, window, cx| {
                ConversationItem::open_visible_panel_thread_in_center(workspace, window, cx)
            })
            .expect("the visible thread should transfer to the center");
        cx.run_until_parked();

        // The item hosts the same live view; nothing was rebuilt.
        cx.read(|cx| {
            assert_eq!(
                item.read(cx).conversation_view().entity_id(),
                view.entity_id()
            );
        });

        // The panel no longer hosts the thread anywhere.
        panel.read_with(cx, |panel, cx| {
            assert_ne!(panel.active_thread_id(cx), Some(thread_id));
            assert!(!panel.is_retained_thread(&thread_id));
        });

        // Exactly one center item hosts it.
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ConversationItem>(cx).count(), 1);
        });
    }

    #[gpui::test]
    async fn test_open_thread_in_center_ignores_empty_draft(cx: &mut TestAppContext) {
        let (panel, workspace, cx) = setup_workspace_with_panel(cx).await;
        panel.update_in(cx, |panel, window, cx| {
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();

        let item = workspace.update_in(cx, |workspace, window, cx| {
            ConversationItem::open_visible_panel_thread_in_center(workspace, window, cx)
        });

        assert!(
            item.is_none(),
            "an empty ephemeral draft has nothing to host in the center"
        );
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ConversationItem>(cx).count(), 0);
        });
    }

    #[gpui::test]
    async fn test_deploy_dedupes_items_per_thread(cx: &mut TestAppContext) {
        init_test(cx);
        let (conversation_view, workspace, cx) = setup_conversation_view(cx).await;

        let (first, second) = workspace.update_in(cx, |workspace, window, cx| {
            let first = ConversationItem::deploy(conversation_view.clone(), workspace, window, cx);
            let second = ConversationItem::deploy(conversation_view.clone(), workspace, window, cx);
            (first, second)
        });
        cx.run_until_parked();

        assert_eq!(
            first.entity_id(),
            second.entity_id(),
            "deploying an already-hosted thread should activate the existing item"
        );
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ConversationItem>(cx).count(), 1);
        });
    }

    #[gpui::test]
    async fn test_move_thread_to_panel_reverses_transfer(cx: &mut TestAppContext) {
        let (panel, workspace, cx) = setup_workspace_with_panel(cx).await;
        open_thread_with_connection(&panel, StubAgentConnection::new(), cx);
        send_message(&panel, cx);

        let view = panel.read_with(cx, |panel, _| {
            panel.active_conversation_view().unwrap().clone()
        });
        let thread_id = active_thread_id(&panel, cx);

        let item = workspace
            .update_in(cx, |workspace, window, cx| {
                ConversationItem::open_visible_panel_thread_in_center(workspace, window, cx)
            })
            .expect("the visible thread should transfer to the center");
        cx.run_until_parked();

        item.update_in(cx, |item, window, cx| {
            item.move_to_panel(window, cx);
        });
        cx.run_until_parked();

        // The center item is gone…
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ConversationItem>(cx).count(), 0);
        });
        // …and the panel hosts the same live view as its visible thread.
        panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.active_thread_id(cx), Some(thread_id));
            assert_eq!(
                panel.active_conversation_view().unwrap().entity_id(),
                view.entity_id()
            );
            assert!(
                !panel.is_retained_thread(&thread_id),
                "the adopted view is the visible thread, not a parked one"
            );
        });
    }

    #[gpui::test]
    async fn test_activate_for_thread_routes_to_center_item(cx: &mut TestAppContext) {
        init_test(cx);
        let (conversation_view, workspace, cx) = setup_conversation_view(cx).await;
        let thread_id = conversation_view.read_with(cx, |view, _| view.thread_id);

        let item = workspace.update_in(cx, |workspace, window, cx| {
            ConversationItem::deploy(conversation_view.clone(), workspace, window, cx)
        });
        // Cover the item with another one so it is no longer active.
        workspace.update_in(cx, |workspace, window, cx| {
            let placeholder = cx.new(|cx| PlaceholderItem {
                focus_handle: cx.focus_handle(),
            });
            workspace.add_item_to_active_pane(Box::new(placeholder), None, true, window, cx);
        });
        cx.run_until_parked();

        let activated = workspace.update_in(cx, |workspace, window, cx| {
            ConversationItem::activate_for_thread(workspace, thread_id, true, window, cx)
        });
        cx.run_until_parked();

        assert!(activated, "the center item should be found and activated");
        let active = workspace
            .read_with(cx, |workspace, cx| workspace.active_item(cx))
            .and_then(|active| active.downcast::<ConversationItem>());
        assert_eq!(
            active.map(|active| active.entity_id()),
            Some(item.entity_id())
        );

        let missing = workspace.update_in(cx, |workspace, window, cx| {
            ConversationItem::activate_for_thread(workspace, ThreadId::new(), true, window, cx)
        });
        assert!(
            !missing,
            "unknown threads fall back to the agent-panel path"
        );
    }

    #[gpui::test]
    async fn test_find_for_session_routes_mentions_to_center_item(cx: &mut TestAppContext) {
        init_test(cx);
        let (conversation_view, workspace, cx) = setup_conversation_view(cx).await;
        let session_id = conversation_view
            .read_with(cx, |view, _| view.root_session_id.clone())
            .expect("the connected stub thread should have a session id");

        let item = workspace.update_in(cx, |workspace, window, cx| {
            ConversationItem::deploy(conversation_view.clone(), workspace, window, cx)
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                ConversationItem::find_for_session(workspace, &session_id, cx)
                    .map(|found| found.entity_id()),
                Some(item.entity_id())
            );
            assert!(
                ConversationItem::find_for_session(workspace, &acp::SessionId::new("unknown"), cx)
                    .is_none(),
                "unknown sessions fall back to the agent-panel path"
            );
        });
    }

    struct PlaceholderItem {
        focus_handle: FocusHandle,
    }

    impl Item for PlaceholderItem {
        type Event = ();

        fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
            "Placeholder".into()
        }
    }

    impl EventEmitter<()> for PlaceholderItem {}

    impl Focusable for PlaceholderItem {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for PlaceholderItem {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            gpui::Empty
        }
    }
}
