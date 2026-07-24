//! Raw AT-SPI characterization: prints each frame's state set, then calls
//! `grab_focus` (Component) and `set_caret_offset` (Text) directly and prints
//! the verbatim results. Diagnoses *why* focus/caret drive fails in a given
//! environment (headless, desktop shell, RAIL) without the mirror in between.

#[cfg(target_os = "linux")]
fn main() {
    use atspi::connection::AccessibilityConnection;
    use atspi::object_ref::ObjectRefOwned;
    use atspi::proxy::accessible::ObjectRefExt;
    use atspi::proxy::component::ComponentProxy;
    use atspi::proxy::text::TextProxy;
    use atspi::zbus::names::BusName;
    use atspi::Role;
    use std::collections::VecDeque;
    use std::time::Duration;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let conn = rt.block_on(AccessibilityConnection::new()).expect("raw connection");

    rt.block_on(async {
        let zconn = conn.connection();
        let registry = conn.root_accessible_on_registry().await.expect("registry root");
        let mut button: Option<ObjectRefOwned> = None;
        let mut text: Option<ObjectRefOwned> = None;

        for app_ref in registry.get_children().await.unwrap_or_default() {
            if app_ref.is_null() {
                continue;
            }
            let Ok(app) = app_ref.as_accessible_proxy(zconn).await else {
                continue;
            };
            let app_name = app.name().await.unwrap_or_default();
            for frame_ref in app.get_children().await.unwrap_or_default() {
                if frame_ref.is_null() {
                    continue;
                }
                let Ok(frame) = frame_ref.as_accessible_proxy(zconn).await else {
                    continue;
                };
                println!(
                    "frame {:?} of app {:?}: states {:?}",
                    frame.name().await.unwrap_or_default(),
                    app_name,
                    frame.get_state().await
                );
                if button.is_some() && text.is_some() {
                    continue;
                }
                let mut queue: VecDeque<ObjectRefOwned> = VecDeque::new();
                queue.push_back(frame_ref);
                let mut seen = 0;
                while let Some(obj) = queue.pop_front() {
                    seen += 1;
                    if seen > 800 || (button.is_some() && text.is_some()) {
                        break;
                    }
                    let Ok(p) = obj.as_accessible_proxy(zconn).await else {
                        continue;
                    };
                    match p.get_role().await.unwrap_or(Role::Invalid) {
                        Role::Button if button.is_none() => button = Some(obj.clone()),
                        Role::Text if text.is_none() => text = Some(obj.clone()),
                        _ => {}
                    }
                    for c in p.get_children().await.unwrap_or_default() {
                        if !c.is_null() {
                            queue.push_back(c);
                        }
                    }
                }
            }
        }

        for (label, target) in [("button", &button), ("text", &text)] {
            let Some(obj) = target else {
                println!("{label}: none found");
                continue;
            };
            let name: BusName = obj.name().expect("named object").clone().into();
            let proxy = obj.as_accessible_proxy(zconn).await.expect("accessible proxy");
            println!(
                "{label} {} {:?}: states {:?}",
                obj.path_as_str(),
                proxy.name().await.unwrap_or_default(),
                proxy.get_state().await
            );
            let component = ComponentProxy::builder(zconn)
                .destination(name.clone())
                .unwrap()
                .path(obj.path().clone())
                .unwrap()
                .build()
                .await;
            match component {
                Ok(component) => {
                    println!("{label} grab_focus -> {:?}", component.grab_focus().await);
                }
                Err(e) => println!("{label} ComponentProxy build -> Err({e:?})"),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            println!("{label} states after -> {:?}", proxy.get_state().await);
        }

        if let Some(obj) = &text {
            let name: BusName = obj.name().expect("named object").clone().into();
            let proxy = TextProxy::builder(zconn)
                .destination(name)
                .unwrap()
                .path(obj.path().clone())
                .unwrap()
                .build()
                .await
                .expect("text proxy");
            println!("text caret_offset -> {:?}", proxy.caret_offset().await);
            println!("text set_caret_offset(3) -> {:?}", proxy.set_caret_offset(3).await);
            tokio::time::sleep(Duration::from_millis(300)).await;
            println!("text caret_offset after -> {:?}", proxy.caret_offset().await);
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {}
