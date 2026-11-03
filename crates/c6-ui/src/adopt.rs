//! Bringing something in where it can have one parent only (work in a
//! project, a session in a work item or a project): the menu lists what is
//! free first, then, past a separator, what is somewhere else already, each
//! with where it is; choosing one of those moves it here.

use leptos::prelude::*;

/// Something to bring in: its id, what to call it, and where it is now
/// (empty: nowhere).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Adoptable {
    pub id: String,
    pub label: String,
    pub parent: String,
}

impl Adoptable {
    pub fn new(id: impl Into<String>, label: impl Into<String>, parent: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), parent: parent.into() }
    }
}

/// The free ones and the ones somewhere else, each in the order given.
pub fn split(items: Vec<Adoptable>) -> (Vec<Adoptable>, Vec<Adoptable>) {
    items.into_iter().partition(|a| a.parent.is_empty())
}

/// What an adopted one says in the menu.
pub fn adopted_label(a: &Adoptable) -> String {
    format!("{} — in {}", a.label, a.parent)
}

/// A menu of things to bring in (see the module): `placeholder` first,
/// then the free ones, a separator and the ones elsewhere. Picking one
/// calls `on_pick` with its id and puts the menu back on the placeholder.
#[component]
pub fn AdoptPick(
    #[prop(into)] placeholder: String,
    #[prop(into)] items: Signal<Vec<Adoptable>>,
    #[prop(into)] on_pick: Callback<String>,
    #[prop(into, optional)] class: String,
    #[prop(into, optional)] title: String,
) -> impl IntoView {
    view! {
        <select class=class title=title data-adopt=""
            prop:value=move || { items.track(); String::new() }
            on:change=move |ev| {
                let id = event_target_value(&ev);
                if !id.is_empty() {
                    on_pick.run(id);
                }
            }>
            <option value="">{placeholder}</option>
            {move || {
                let (free, taken) = split(items.get());
                let gap = !taken.is_empty();
                view! {
                    {free.into_iter().map(|a| view! { <option value=a.id>{a.label}</option> }).collect_view()}
                    {gap.then(|| view! { <option disabled=true value="">"──── elsewhere: choosing one moves it here ────"</option> })}
                    {taken.into_iter().map(|a| {
                        let tip = format!("Moves it here, out of {}", a.parent);
                        view! { <option value=a.id.clone() title=tip>{adopted_label(&a)}</option> }
                    }).collect_view()}
                }
            }}
        </select>
    }
}

/// Sessions to bring in, from the live list: top-level ones in `realm`
/// (not subagents, side chats or mirrored ones), each with where it is:
/// the work it serves, or the project it's a quick session of (a tag with
/// no work). `here` leaves out the ones already where they'd go.
pub fn sessions(live: &[c6_api::Session], realm: &str, here: impl Fn(&c6_api::Session) -> bool) -> Vec<Adoptable> {
    live.iter()
        .filter(|s| s.parent_id.is_none() && !s.observed && !s.on_head && s.realm == realm && !here(s))
        .map(|s| {
            let parent = match &s.work {
                Some(w) if w.id.is_empty() => w.project_title.clone(),
                Some(w) => w.title.clone(),
                None => String::new(),
            };
            let title = if s.title.trim().is_empty() { "Untitled session".to_owned() } else { s.title.clone() };
            Adoptable::new(s.id.clone(), title, parent)
        })
        .collect()
}

/// Takes a session out of the work it serves, so it can move: a session
/// serves one work item, and one serving work can't be a quick session.
/// (A quick session leaves its project by itself when it joins work or
/// another project.)
pub async fn free_session(session: &str) -> Result<(), String> {
    let reference = format!("session:{session}");
    for w in crate::api::work_of(reference.clone()).await? {
        crate::api::work_detach(w.id, reference.clone()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_ones_come_first_and_adopted_ones_say_where_they_are() {
        let items = vec![
            Adoptable::new("a", "In realms", "Realms rollout"),
            Adoptable::new("b", "Free one", ""),
            Adoptable::new("c", "Free two", ""),
        ];
        let (free, taken) = split(items);
        assert_eq!(free.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(taken.iter().map(adopted_label).collect::<Vec<_>>(), ["In realms — in Realms rollout"]);
    }
}
