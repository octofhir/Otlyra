//! What a form sends, against markup rather than against forty calls to the
//! mutator.
//!
//! An integration test rather than a unit one, because building the document the
//! way a page builds it needs the parser, and the parser is built on this crate.

use otlyra_dom::submit::{Entry, Method, entry_list, submission};
use otlyra_dom::{Document, FormState, NodeId};

fn parse(html: &str) -> Document {
    otlyra_html::parse(html.as_bytes(), Some("utf-8")).document
}

fn node_with_id(document: &Document, id: &str) -> NodeId {
    let mut order = Vec::new();
    let mut stack = vec![document.root()];
    while let Some(node) = stack.pop() {
        order.push(node);
        stack.extend(document.children(node));
    }
    order
        .into_iter()
        .find(|&node| {
            document
                .get(node)
                .and_then(|inner| inner.element())
                .and_then(|element| element.id())
                == Some(id)
        })
        .unwrap_or_else(|| panic!("no element with id {id}"))
}

#[test]
fn only_what_a_form_actually_holds_is_sent() {
    let document = parse(
        "<form id=f>\
         <input name=who value=Ada>\
         <input name=off type=checkbox>\
         <input name=on type=checkbox checked>\
         <input name=pick type=radio value=a checked>\
         <input name=pick type=radio value=b>\
         <input name=gone value=x disabled>\
         <input value=nameless>\
         <textarea name=note>hello</textarea>\
         <select name=choice><option>one<option selected>two</select>\
         <button name=go value=send>Send</button>\
         <button name=other value=nope>Other</button>\
         </form>",
    );
    let state = FormState::new();
    let form = node_with_id(&document, "f");
    let entries = entry_list(&document, &state, form, None);
    assert_eq!(
        entries,
        vec![
            Entry::text("who", "Ada"),
            Entry::text("on", "on"),
            Entry::text("pick", "a"),
            Entry::text("note", "hello"),
            Entry::text("choice", "two"),
        ]
    );
}

/// A form with two buttons has to say which was used, so only the one that was
/// pressed sends its own pair.
#[test]
fn the_button_that_was_pressed_is_the_only_one_that_is_sent() {
    let document = parse(
        "<form id=f><button id=go name=go value=send>Send</button>\
         <button name=other value=nope>Other</button></form>",
    );
    let state = FormState::new();
    let form = node_with_id(&document, "f");
    let go = node_with_id(&document, "go");
    assert_eq!(
        entry_list(&document, &state, form, Some(go)),
        vec![Entry::text("go", "send")]
    );
}

#[test]
fn a_get_puts_the_pairs_in_the_address_and_replaces_the_query() {
    let document =
        parse("<form id=f action=\"/search?page=2#top\"><input name=q value=\"a b&c\"></form>");
    let state = FormState::new();
    let form = node_with_id(&document, "f");
    let sent = submission(&document, &state, form, None);
    assert_eq!(sent.method, Method::Get);
    assert_eq!(sent.url, "/search?q=a+b%26c#top");
    assert!(sent.body.is_empty());
}

#[test]
fn a_post_puts_them_in_the_body() {
    let document =
        parse("<form id=f method=post action=/save><input name=who value=\"Ada L\"></form>");
    let state = FormState::new();
    let form = node_with_id(&document, "f");
    let sent = submission(&document, &state, form, None);
    assert_eq!(sent.method, Method::Post);
    assert_eq!(sent.url, "/save");
    assert_eq!(String::from_utf8_lossy(&sent.body), "who=Ada+L");
    assert_eq!(sent.content_type, "application/x-www-form-urlencoded");
}

#[test]
fn multipart_cuts_the_body_at_a_boundary() {
    let document = parse(
        "<form id=f method=post enctype=multipart/form-data action=/save>\
         <input name=who value=Ada></form>",
    );
    let state = FormState::new();
    let form = node_with_id(&document, "f");
    let sent = submission(&document, &state, form, None);
    let body = String::from_utf8_lossy(&sent.body).into_owned();
    assert!(
        sent.content_type
            .starts_with("multipart/form-data; boundary=")
    );
    assert!(body.contains("Content-Disposition: form-data; name=\"who\"\r\n\r\nAda\r\n"));
    assert!(body.ends_with("--\r\n"));
}

#[test]
fn a_pressed_button_can_send_the_form_somewhere_else() {
    let document = parse(
        "<form id=f action=/one method=get>\
         <input name=q value=x>\
         <button id=go formaction=/two formmethod=post>Go</button></form>",
    );
    let state = FormState::new();
    let form = node_with_id(&document, "f");
    let go = node_with_id(&document, "go");
    let sent = submission(&document, &state, form, Some(go));
    assert_eq!(sent.method, Method::Post);
    assert_eq!(sent.url, "/two");
}

/// A form carrying a file sends the file: its name, what it is, and its bytes.
#[test]
fn a_file_is_sent_with_its_name_and_its_bytes() {
    let document = parse(
        "<form id=f method=post enctype=multipart/form-data action=/upload>\
         <input name=note value=hello><input id=doc name=doc type=file></form>",
    );
    let mut state = FormState::new();
    let form = node_with_id(&document, "f");
    let picker = node_with_id(&document, "doc");

    // Nothing chosen: still one part, and an empty file rather than empty text.
    let empty = submission(&document, &state, form, None);
    let body = String::from_utf8_lossy(&empty.body).into_owned();
    assert!(body.contains("name=\"doc\"; filename=\"\""), "{body}");

    state.set_files(
        picker,
        vec![otlyra_dom::form::ChosenFile {
            name: "notes.txt".to_owned(),
            media_type: "text/plain".to_owned(),
            bytes: b"one\r\ntwo".to_vec(),
        }],
    );
    let sent = submission(&document, &state, form, None);
    let body = String::from_utf8_lossy(&sent.body).into_owned();
    assert!(body.contains("Content-Disposition: form-data; name=\"note\"\r\n\r\nhello"));
    assert!(body.contains(
        "Content-Disposition: form-data; name=\"doc\"; filename=\"notes.txt\"\r\n\
         Content-Type: text/plain\r\n\r\none\r\ntwo"
    ));
    assert!(
        sent.content_type
            .starts_with("multipart/form-data; boundary=")
    );
}

/// Sent any other way, a file is its name and nothing else — which is what the
/// specification says happens to a form that carries one and does not say so.
#[test]
fn a_file_in_a_urlencoded_form_is_only_its_name() {
    let document =
        parse("<form id=f method=post action=/save><input id=doc name=doc type=file></form>");
    let mut state = FormState::new();
    let form = node_with_id(&document, "f");
    let picker = node_with_id(&document, "doc");
    state.set_files(
        picker,
        vec![otlyra_dom::form::ChosenFile {
            name: "notes.txt".to_owned(),
            media_type: "text/plain".to_owned(),
            bytes: b"secret".to_vec(),
        }],
    );
    let sent = submission(&document, &state, form, None);
    assert_eq!(String::from_utf8_lossy(&sent.body), "doc=notes.txt");
}
