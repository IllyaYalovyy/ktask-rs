//! Event handling for the read-only provider catalogue screen.

use crate::providers::ProvidersScreen;

use super::{App, Event, Tried, handled};

/// `event`, applied to the read-only providers screen when it owns it.
pub(super) fn try_providers(mut app: App, event: Event) -> Tried {
    match event {
        Event::Key(key) if app.providers.is_some() => {
            let Some(screen) = app.providers.take() else {
                return handled(app);
            };
            let (screen, close, check) = screen.key(key);
            if !close {
                app.providers = Some(screen);
            }
            if let Some(name) = check {
                app.provider_check_requested = Some(name);
            }
            handled(app)
        }
        Event::ProvidersLoaded(providers) => handled(App {
            providers: Some(ProvidersScreen::new(providers)),
            ..app
        }),
        Event::ProviderChecked(check) => {
            app.providers = app.providers.map(|screen| screen.checked(check));
            handled(app)
        }
        other => Tried::Unhandled(Box::new(app), Box::new(other)),
    }
}
