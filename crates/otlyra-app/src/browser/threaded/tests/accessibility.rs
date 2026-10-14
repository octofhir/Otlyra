use super::*;

struct IgnoreChanges;
impl accesskit_consumer::TreeChangeHandler for IgnoreChanges {
    fn node_added(&mut self, _: &accesskit_consumer::NodeRef) {}
    fn node_updated(&mut self, _: &accesskit_consumer::NodeRef, _: &accesskit_consumer::NodeRef) {}
    fn focus_moved(
        &mut self,
        _: Option<&accesskit_consumer::NodeRef>,
        _: Option<&accesskit_consumer::NodeRef>,
    ) {
    }
    fn node_removed(&mut self, _: &accesskit_consumer::NodeRef) {}
}

fn apply(tree: &mut accesskit_consumer::Tree, window: &mut WindowBrowser) {
    if let Some(update) = window.accessibility() {
        tree.update_and_process_changes(update, &mut IgnoreChanges);
    }
}

#[test]
fn address_edits_publish_chrome_nodes_and_reuse_the_owners_document() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = fields_window(viewport);
    window.compose(viewport);
    let mut tree = accesskit_consumer::Tree::new(window.accessibility().unwrap(), true);
    key(&mut window, 'l');
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    window.compose(viewport);
    apply(&mut tree, &mut window);
    let document = Arc::clone(&window.snapshot.document);
    for character in "responsive".chars() {
        window.on_event(PlatformEvent::TextInput(character));
        window.compose(viewport);
        let update = window.accessibility().unwrap();
        assert!(
            update
                .nodes
                .iter()
                .all(|(id, node)| crate::a11y::described_index(*id).is_some()
                    || *id == accesskit::NodeId(0) && node.role() == accesskit::Role::Window),
            "an address edit resent document nodes: {:?}",
            update
                .nodes
                .iter()
                .map(|(id, node)| (id, node.role(), node.label(), node.value()))
                .collect::<Vec<_>>()
        );
        tree.update_and_process_changes(update, &mut IgnoreChanges);
        let serial = window.serial;
        wait(&mut window, |window| window.snapshot.serial >= serial);
        assert!(Arc::ptr_eq(&document, &window.snapshot.document));
        assert!(window.accessibility().is_none());
    }
    assert_eq!(
        tree.state().focus_in_tree().data().value(),
        Some("responsive")
    );
    assert_eq!(
        tree.state().focus_in_tree().data().role(),
        accesskit::Role::TextInput
    );
}

#[test]
fn pending_navigation_drops_old_accessible_nodes_before_content_replies() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = fields_window(viewport);
    window.compose(viewport);
    let mut tree = accesskit_consumer::Tree::new(window.accessibility().unwrap(), true);
    let resume = pause_owner(&window);
    window.apply(UiAction::Navigate("https://next.example/".into()));
    apply(&mut tree, &mut window);
    let document = tree
        .state()
        .root()
        .children()
        .find(|node| node.data().role() == accesskit::Role::Document)
        .unwrap();
    assert_eq!(document.children().count(), 0);
    drop(resume);
    let serial = window.serial;
    wait(&mut window, |window| {
        window.snapshot.serial >= serial
            && window.tabs[window.active].url == "https://next.example/"
    });
    window.compose(viewport);
    apply(&mut tree, &mut window);
    let document = tree
        .state()
        .root()
        .children()
        .find(|node| node.data().role() == accesskit::Role::Document)
        .unwrap();
    assert!(document.children().next().is_some());
}

#[test]
fn tab_projection_keeps_accessibility_valid_while_the_owner_is_busy() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = fields_window(viewport);
    window.compose(viewport);
    let mut tree = accesskit_consumer::Tree::new(window.accessibility().unwrap(), true);
    let _resume = pause_owner(&window);
    key(&mut window, 't');
    window.compose(viewport);
    apply(&mut tree, &mut window);
    assert_eq!(
        tree.state().focus_in_tree().data().role(),
        accesskit::Role::TextInput
    );
    let document = tree
        .state()
        .root()
        .children()
        .find(|node| node.data().role() == accesskit::Role::Document)
        .unwrap();
    assert_eq!(document.children().count(), 0);
    window.select(0);
    window.compose(viewport);
    apply(&mut tree, &mut window);
    let document = tree
        .state()
        .root()
        .children()
        .find(|node| node.data().role() == accesskit::Role::Document)
        .unwrap();
    assert!(document.children().next().is_some());
    assert_eq!(
        tree.state().focus_in_tree().data().value(),
        Some("prepared")
    );
}

#[test]
fn an_unchanged_wake_and_page_hover_publish_no_accessibility_update() {
    let viewport = Viewport::new(900, 640, 1.0);
    let mut window = window(viewport, "https://page.example/");
    ready(&mut window);
    window.compose(viewport);
    window.accessibility().unwrap();
    window.on_event(PlatformEvent::Woken);
    assert!(window.accessibility().is_none());
    window.on_event(PlatformEvent::PointerMoved { x: 800.0, y: 500.0 });
    let serial = window.serial;
    wait(&mut window, |window| window.snapshot.serial >= serial);
    assert!(window.accessibility().is_none());
}
