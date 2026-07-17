//! The seam between a [`ConversationView`] and the surface presenting it.
//!
//! A conversation is presented either by the agent panel
//! ([`AgentPanel`](crate::AgentPanel), the dock every thread starts in) or by
//! a [`ConversationItem`](crate::ConversationItem) in the center pane. The
//! view's notification machinery needs exactly three things from whichever
//! host it currently lives in, captured by this trait; each host implements
//! it in its own module.

use std::rc::Rc;

use gpui::{App, Context, Entity, SharedString, Subscription, Window};
use workspace::PathList;

use crate::Agent;
use crate::conversation_view::ConversationView;
use crate::thread_metadata_store::ThreadId;

/// Invoked when the host's presentation state changed in a way that may have
/// made the subscribing view newly visible (used to auto-dismiss notification
/// pop-ups the moment the user can see the thread).
pub(crate) type VisibilityChangedCallback =
    Rc<dyn Fn(&ConversationView, &mut Window, &mut Context<ConversationView>)>;

/// A surface that presents a [`ConversationView`] to the user.
pub(crate) trait ConversationHost {
    /// Whether this host is currently presenting `view` on screen. Window
    /// activation (and, in a multi-workspace window, which workspace is
    /// foregrounded) is the caller's concern, not the host's.
    fn is_view_visible(&self, view: &Entity<ConversationView>, cx: &App) -> bool;

    /// Bring the identified thread to the foreground in this host: reveal the
    /// host's surface, make the thread its active view, and focus it.
    fn reveal_thread(
        &self,
        agent: Agent,
        thread_id: ThreadId,
        work_dirs: Option<PathList>,
        title: Option<SharedString>,
        window: &mut Window,
        cx: &mut App,
    );

    /// Subscribe the view owning `cx` to host-side changes that can newly
    /// reveal it, invoking `on_change` for each one. Returns `None` when the
    /// host has nothing to observe.
    fn subscribe_to_visibility_changes(
        &self,
        window: &Window,
        cx: &mut Context<ConversationView>,
        on_change: VisibilityChangedCallback,
    ) -> Option<Subscription>;
}
