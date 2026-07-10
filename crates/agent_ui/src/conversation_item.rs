//! Hosts a live [`ConversationView`] as a center-pane workspace [`Item`].
//!
//! The agent panel remains the coordinator that creates threads (it owns the
//! `AgentConnectionStore`, `ThreadStore`, and `fs`); this wrapper only renders
//! an already-created view in a pane. A thread is hosted in exactly one place
//! at a time, so the item is never cloned on split.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, App, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Window, prelude::*, pulsating_between,
};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{Tab, prelude::*, utils::WithRemSize};
use workspace::{Item, item::ItemEvent};

use crate::conversation_view::ConversationView;

pub struct ConversationItem {
    view: Entity<ConversationView>,
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

        Self {
            view,
            _subscriptions,
        }
    }

    pub fn conversation_view(&self) -> &Entity<ConversationView> {
        &self.view
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
            .child(self.render_title(cx));

        let content = v_flex()
            .size_full()
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
    use crate::test_support::StubAgentServer;
    use crate::{Agent, AgentThreadSource};
    use agent::ThreadStore;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
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
}
