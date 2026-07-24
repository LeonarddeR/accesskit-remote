# AT-SPI Mirror Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the three open mirror follow-ups from `docs/next-steps.md`: LibreOffice Writer paragraph text runs, Calc active-descendant for lazy grid cells (on-demand splice), and container bounds via `Component.GetExtents` plus the empty-field caret anchor.

## Context

`accesskit_remote_atspi` mirrors a Linux AT-SPI tree into AccessKit `TreeUpdate`s consumed on Windows via UIA. Installing LibreOffice as a rich test target exposed three gaps: (i) Writer's document body (`document text` → `paragraph` leaves) gets no TextRuns because `Role::Paragraph` is in neither text-role set; (ii) Calc fires `object:active-descendant-changed` on cell navigation but the cell was never walked (lazy grid), so `resolve_focus_target` fails and the event is dropped; (iii) non-text nodes carry no bounds and an empty text field's run has no geometry, so a caret cannot be placed on it. User-confirmed scope: these three items, with **on-demand resolution** (not rewalk-fallback) for Calc. RTL direction, the `GetForegroundWindow` gate, and same-title disambiguation stay out of scope.

**Architecture:** Pure logic stays in `mapping.rs` (bus-free, unit-tested), bus I/O in `mirror.rs`, state/orchestration in `source.rs` — the crate's existing split. The Calc item factors a `read_node` helper out of the walk, adds a pure chain-splice in `mapping.rs`, and guards the rewalk path against dropping a spliced focused node. Every stage is red → green → live-verify → commit.

**Tech Stack:** Rust; atspi 0.30 / zbus 5.18 / tokio (current-thread); dev-dep `accesskit_consumer 0.38` for round-trip tests. Crate is `#![cfg(target_os = "linux")]` — build/test via WSL only.

## Global Constraints

- Test command (run from Windows; judge by output, **`wsl -e bash -lc` exits 15 even on success**):
  `wsl -e bash -lc 'cd /mnt/p/accesskit-remote && CARGO_TARGET_DIR=~/target-accesskit-remote cargo test -p accesskit_remote_atspi 2>&1 | tail -25'`
- Never `--workspace` on Linux; always `-p accesskit_remote_atspi`; keep `CARGO_TARGET_DIR=~/target-accesskit-remote`.
- Red-green TDD per stage: write the failing test, see it fail, implement minimally, see it pass, live-verify where the environment allows, commit (user's standing instruction: commit each tested component without asking).
- Code comments: describe behavior only — no rationale/history (user's global instruction).
- Commit message style from git log: lowercase `atspi:` prefix, imperative summary.
- Live-verify recipes: a11y enable + `setsid` launches per `docs/next-steps.md` Workflow notes; kill LibreOffice with `pkill -x soffice.bin`, never `pkill -f` from a script mentioning the name.
- Current test count is 44 in this crate; every stage must keep the full suite green.

## Execution notes (user-required)

- **Model selection:** Tasks 1, 6, 7 are mechanical/pattern-following — delegate to a cheap model (sonnet). Tasks 3, 4, 5 are design- and invariant-sensitive (tree-splice correctness, consumer-fatal focus rules, async plumbing) — keep in the main loop or use the top model. Task 2 is a pure refactor — sonnet. All live verification (WSL recipes are fragile) stays in the main loop.
- On approval, copy this plan to `docs/superpowers/plans/2026-07-24-atspi-mirror-follow-ups.md` so the repo records it.

---

### Task 1: Writer Paragraph text runs

**Files:**
- Modify: `crates/accesskit_remote_atspi/src/mapping.rs` (role set at :199, tests mod at :616)

**Interfaces:**
- Consumes: `reads_text_runs(role, has_element_children)` (mapping.rs:217), `has_text_caret` (:224), `build_window_update` (:505), test helpers `leaf` (:620) and `ext` (:1153).
- Produces: `reads_text_runs(Role::Paragraph, false) == true`; no signature changes.

- [ ] **Step 1: Write the failing test** (mapping.rs tests mod, near `reads_text_runs_editable_always_static_only_as_leaf`)

```rust
    #[test]
    fn paragraph_reads_runs_as_leaf_and_stays_caret_less() {
        assert!(reads_text_runs(Role::Paragraph, false));
        assert!(!reads_text_runs(Role::Paragraph, true));
        assert!(!has_text_caret(Role::Paragraph));
    }
```

Also add the consumer contract pin (goes green immediately — it pins the `ExcludeNode` traversal this item depends on: the *Document* ancestor's TextPattern spans its paragraphs' runs because non-TextRun children are filter-transparent; `MirrorNode.text` is hand-supplied here, so it bypasses the walk gate):

```rust
    #[test]
    fn consumer_reads_document_text_through_paragraph_runs() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/doc".into()];
        let mut doc = leaf("/doc", Role::DocumentText, "");
        doc.children = vec!["/doc/p1".into(), "/doc/p2".into()];
        let mut p1 = leaf("/doc/p1", Role::Paragraph, "");
        p1.text = Some(TextState {
            text: "One.".into(),
            caret: None,
            selection: None,
            extents: None,
        });
        let mut p2 = leaf("/doc/p2", Role::Paragraph, "");
        p2.text = Some(TextState {
            text: "Two.".into(),
            caret: None,
            selection: None,
            extents: None,
        });

        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root, doc, p1, p2], &mut ids, &mut HashMap::new());
        let doc_id = ids.get("/doc").unwrap();

        let tree = accesskit_consumer::Tree::new(update, false);
        let state = tree.state();
        let doc_node = state
            .node_by_tree_local_id(doc_id, accesskit::TreeId::ROOT)
            .expect("document present in consumer tree");
        assert!(doc_node.supports_text_ranges());
        assert_eq!(doc_node.document_range().text(), "One.Two.");
    }
```

- [ ] **Step 2: Run tests, verify the role test fails**

Run: `wsl -e bash -lc 'cd /mnt/p/accesskit-remote && CARGO_TARGET_DIR=~/target-accesskit-remote cargo test -p accesskit_remote_atspi paragraph -- --nocapture 2>&1 | tail -15'`
Expected: `paragraph_reads_runs_as_leaf_and_stays_caret_less ... FAILED` (assertion `reads_text_runs(Role::Paragraph, false)`); the consumer test PASSES (contract pin).

- [ ] **Step 3: Minimal implementation** — one arm added to `is_static_text_role` (mapping.rs:199); update its doc comment to mention paragraphs:

```rust
/// Whether a role is static text: a label, terminal, document, or paragraph.
fn is_static_text_role(role: Role) -> bool {
    matches!(
        role,
        Role::Label
            | Role::Terminal
            | Role::Paragraph
            | Role::DocumentFrame
            | Role::DocumentText
            | Role::DocumentWeb
            | Role::DocumentEmail
            | Role::DocumentSpreadsheet
            | Role::DocumentPresentation
    )
}
```

Nothing else changes: `map_role` already maps `Paragraph → accesskit::Role::Paragraph` (:160); the walk gate at mirror.rs:175 (`reads_text_runs(role, !children.is_empty()) && Interface::Text present`) now covers paragraph leaves; a paragraph with inline element children (e.g. a Link) keeps its structure via the leaf gate; `has_text_caret` stays false (caret-less like Label — the selectable-document caret remains the known deferred item).

- [ ] **Step 4: Run the full suite, verify green** (Global Constraints command). Expected: all tests pass (46 now).

- [ ] **Step 5: Live-verify on Writer (gtk3), record walk cost**

```
wsl -e bash -lc 'busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true; SAL_USE_VCLPLUGIN=gtk3 LIBGL_ALWAYS_SOFTWARE=1 setsid soffice --writer --norestore >/tmp/lo.log 2>&1 </dev/null & sleep 12; cd /mnt/p/accesskit-remote && CARGO_TARGET_DIR=~/target-accesskit-remote time cargo run -p accesskit_remote_atspi --example dump_tree 2>/dev/null | grep -c Paragraph'
```

Expected: a nonzero Paragraph count; then re-run piping through `grep -A2 Paragraph | head -30` to confirm paragraphs carry one run each with `sel=None` and plausible geometry. **Record the `time` output** — the walk-cost datum for Task 6's risk assessment (each paragraph adds 1 bus call/char, ≤512/node). Kill: `wsl -e bash -lc 'pkill -x soffice.bin'`.

- [ ] **Step 6: Commit**

```bash
git add crates/accesskit_remote_atspi/src/mapping.rs
git commit -m "atspi: mirror Paragraph leaves into static text runs"
```

---

### Task 2: factor `read_node` out of `walk_window`

Pure refactor — no behavior change; the shared per-object reader is the seam Tasks 4 and 6 plug into.

**Files:**
- Modify: `crates/accesskit_remote_atspi/src/mirror.rs:140-195`

**Interfaces:**
- Consumes: the inline per-object read in `walk_window` (mirror.rs:157-192), `read_text_state` (:204).
- Produces: `pub(crate) async fn read_node(zconn: &atspi::zbus::Connection, obj: &ObjectRefOwned) -> Option<(MirrorNode, Vec<ObjectRefOwned>)>` — Tasks 4 and 6 rely on this exact signature.

- [ ] **Step 1: Extract the helper** (below `walk_window`):

```rust
/// Reads one AT-SPI object into a [`MirrorNode`] plus its non-null child
/// refs. `None` only when the object's proxy cannot be built; individual
/// property failures degrade to the same defaults the walk has always used.
pub(crate) async fn read_node(
    zconn: &atspi::zbus::Connection,
    obj: &ObjectRefOwned,
) -> Option<(MirrorNode, Vec<ObjectRefOwned>)> {
    let proxy = obj.as_accessible_proxy(zconn).await.ok()?;
    let role = proxy.get_role().await.unwrap_or(Role::Invalid);
    let name = proxy.name().await.unwrap_or_default();
    let states = proxy.get_state().await.unwrap_or_else(|_| StateSet::empty());
    let interfaces = proxy.get_interfaces().await.ok();
    let actionable = interfaces
        .as_ref()
        .is_some_and(|set| set.contains(Interface::Action));
    let mut children = Vec::new();
    let mut child_refs = Vec::new();
    for child in proxy.get_children().await.unwrap_or_default() {
        if child.is_null() {
            continue;
        }
        children.push(child.path_as_str().to_owned());
        child_refs.push(child);
    }
    let text = if reads_text_runs(role, !children.is_empty())
        && interfaces.as_ref().is_some_and(|set| set.contains(Interface::Text))
    {
        read_text_state(zconn, obj, has_text_caret(role), true).await
    } else {
        None
    };
    let node = MirrorNode {
        path: obj.path_as_str().to_owned(),
        role,
        name,
        focusable: states.contains(State::Focusable),
        focused: states.contains(State::Focused),
        actionable,
        children,
        text,
    };
    Some((node, child_refs))
}
```

`walk_window`'s loop body becomes:

```rust
    while let Some(obj) = queue.pop_front() {
        if nodes.len() >= MAX_NODES_PER_WINDOW {
            break;
        }
        let path = obj.path_as_str().to_owned();
        if objects.contains_key(&path) {
            continue;
        }
        let Some((node, child_refs)) = read_node(zconn, &obj).await else {
            continue;
        };
        queue.extend(child_refs);
        nodes.push(node);
        objects.insert(path, obj);
    }
```

- [ ] **Step 2: Run the full suite** (Global Constraints command). Expected: all green — bus code has no unit tests; the suite proves the pure layer untouched.

- [ ] **Step 3: Smoke-verify unchanged walk** — gnome-text-editor recipe (a11y enable, `GSK_RENDERER=cairo LIBGL_ALWAYS_SOFTWARE=1 setsid gnome-text-editor`, sleep ~7s, `cargo run --example dump_tree`); expect the usual node count (~130 TextRun-era, exact number irrelevant — must match a pre-change run if in doubt: stash A/B).

- [ ] **Step 4: Commit**

```bash
git add crates/accesskit_remote_atspi/src/mirror.rs
git commit -m "atspi: factor the walk's per-object read into read_node"
```

---

### Task 3: pure splice machinery (`mapping.rs`)

**Files:**
- Modify: `crates/accesskit_remote_atspi/src/mapping.rs` (new public functions near `build_window_update`; tests)

**Interfaces:**
- Consumes: private `build_node` (:557), `NodeIdMap`, `TextNodeCache`, `MirrorNode`.
- Produces (Tasks 4/5 rely on these exact signatures):
  - `pub fn emitted_children(nodes: &[MirrorNode]) -> HashMap<String, Vec<String>>`
  - `pub struct SpliceResult { pub update: TreeUpdate, pub children: Vec<(String, Vec<String>)> }`
  - `pub fn splice_chain_update(chain: &[MirrorNode], ancestor_children: &[String], known: &HashSet<String>, ids: &mut NodeIdMap, text_caches: &mut HashMap<String, TextNodeCache>) -> Option<SpliceResult>`
  - `pub fn merge_update(full: &mut TreeUpdate, splice: TreeUpdate)`

- [ ] **Step 1: Write the failing tests** (mapping.rs tests mod; new section `// --- Chain splicing ---`):

```rust
    // --- Chain splicing ---

    #[test]
    fn emitted_children_filters_to_walked_set() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/a".into(), "/lazy".into()];
        let a = leaf("/a", Role::Panel, "");
        let map = emitted_children(&[root, a]);
        assert_eq!(map["/win"], vec!["/a".to_owned()]);
        assert_eq!(map["/a"], Vec::<String>::new());
    }

    #[test]
    fn splice_appends_chain_under_known_ancestor() {
        let mut fresh_table = leaf("/table", Role::Table, "grid");
        fresh_table.children = vec!["/table/cell".into()];
        let cell = leaf("/table/cell", Role::TableCell, "A1");
        let known: HashSet<String> = ["/win".to_owned(), "/table".to_owned()].into();
        let mut ids = NodeIdMap::new();
        let table_id = ids.id_for("/table");

        let result = splice_chain_update(
            &[fresh_table, cell],
            &[],
            &known,
            &mut ids,
            &mut HashMap::new(),
        )
        .expect("chain splices");

        let cell_id = ids.get("/table/cell").expect("cell id allocated");
        assert_eq!(result.update.focus, cell_id);
        assert!(result.update.tree.is_none());
        let (_, table_node) = result
            .update
            .nodes
            .iter()
            .find(|(id, _)| *id == table_id)
            .expect("ancestor re-emitted");
        assert!(table_node.children().contains(&cell_id));
        assert!(result.update.nodes.iter().any(|(id, _)| *id == cell_id));
        assert_eq!(
            result.children,
            vec![
                ("/table".to_owned(), vec!["/table/cell".to_owned()]),
                ("/table/cell".to_owned(), Vec::new()),
            ]
        );
    }

    #[test]
    fn splice_preserves_ancestor_children_absent_from_fresh_read() {
        let mut fresh_table = leaf("/table", Role::Table, "grid");
        fresh_table.children = vec!["/table/cell".into()];
        let cell = leaf("/table/cell", Role::TableCell, "A1");
        let known: HashSet<String> =
            ["/table".to_owned(), "/table/a".to_owned(), "/table/b".to_owned()].into();
        let mut ids = NodeIdMap::new();
        let a_id = ids.id_for("/table/a");
        let b_id = ids.id_for("/table/b");

        let result = splice_chain_update(
            &[fresh_table, cell],
            &["/table/a".to_owned(), "/table/b".to_owned()],
            &known,
            &mut ids,
            &mut HashMap::new(),
        )
        .expect("chain splices");

        let table_id = ids.get("/table").unwrap();
        let cell_id = ids.get("/table/cell").unwrap();
        let (_, table_node) = result
            .update
            .nodes
            .iter()
            .find(|(id, _)| *id == table_id)
            .unwrap();
        assert_eq!(table_node.children(), &[a_id, b_id, cell_id]);
    }

    #[test]
    fn splice_ignores_unknown_fresh_children() {
        let mut fresh_table = leaf("/table", Role::Table, "grid");
        fresh_table.children =
            vec!["/table/x1".into(), "/table/cell".into(), "/table/x2".into()];
        let cell = leaf("/table/cell", Role::TableCell, "A1");
        let known: HashSet<String> = ["/table".to_owned()].into();
        let mut ids = NodeIdMap::new();

        let result =
            splice_chain_update(&[fresh_table, cell], &[], &known, &mut ids, &mut HashMap::new())
                .expect("chain splices");

        let table_id = ids.get("/table").unwrap();
        let cell_id = ids.get("/table/cell").unwrap();
        let (_, table_node) = result
            .update
            .nodes
            .iter()
            .find(|(id, _)| *id == table_id)
            .unwrap();
        assert_eq!(table_node.children(), &[cell_id], "never-walked cells contribute nothing");
        assert!(ids.get("/table/x1").is_none());
    }

    #[test]
    fn splice_injects_missing_interior_link() {
        let table = leaf("/table", Role::Table, "grid");
        let row = leaf("/table/row", Role::Panel, "");
        let cell = leaf("/table/row/cell", Role::TableCell, "A1");
        let known: HashSet<String> = ["/table".to_owned()].into();
        let mut ids = NodeIdMap::new();

        let result = splice_chain_update(
            &[table, row, cell],
            &[],
            &known,
            &mut ids,
            &mut HashMap::new(),
        )
        .expect("chain splices");

        let row_id = ids.get("/table/row").unwrap();
        let cell_id = ids.get("/table/row/cell").unwrap();
        let (_, row_node) = result
            .update
            .nodes
            .iter()
            .find(|(id, _)| *id == row_id)
            .expect("interior node emitted");
        assert_eq!(row_node.children(), &[cell_id]);
    }

    #[test]
    fn resplice_is_idempotent() {
        let known: HashSet<String> = ["/table".to_owned()].into();
        let mut ids = NodeIdMap::new();
        let build = |ids: &mut NodeIdMap| {
            let mut fresh_table = leaf("/table", Role::Table, "grid");
            fresh_table.children = vec!["/table/cell".into()];
            let cell = leaf("/table/cell", Role::TableCell, "A1");
            splice_chain_update(
                &[fresh_table, cell],
                &[],
                &known,
                ids,
                &mut HashMap::new(),
            )
            .expect("chain splices")
        };
        let first = build(&mut ids);
        let second = build(&mut ids);
        assert_eq!(first.update.focus, second.update.focus);
        assert_eq!(first.children, second.children);
        let ids_of = |r: &SpliceResult| {
            let mut v: Vec<_> = r.update.nodes.iter().map(|(id, _)| *id).collect();
            v.sort();
            v
        };
        assert_eq!(ids_of(&first), ids_of(&second));
    }

    #[test]
    fn splice_rejects_a_short_chain() {
        let table = leaf("/table", Role::Table, "grid");
        let known: HashSet<String> = ["/table".to_owned()].into();
        let mut ids = NodeIdMap::new();
        assert!(splice_chain_update(&[table], &[], &known, &mut ids, &mut HashMap::new())
            .is_none());
        assert!(splice_chain_update(&[], &[], &known, &mut ids, &mut HashMap::new()).is_none());
    }

    #[test]
    fn spliced_text_node_builds_runs_and_cache() {
        let mut fresh_doc = leaf("/doc", Role::DocumentText, "");
        fresh_doc.children = vec!["/doc/p".into()];
        let mut p = leaf("/doc/p", Role::Paragraph, "");
        p.text = Some(TextState {
            text: "hi".into(),
            caret: None,
            selection: None,
            extents: None,
        });
        let known: HashSet<String> = ["/doc".to_owned()].into();
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();

        let result = splice_chain_update(&[fresh_doc, p], &[], &known, &mut ids, &mut caches)
            .expect("chain splices");

        let run_id = ids.get("/doc/p#run0").expect("run id allocated");
        assert!(result.update.nodes.iter().any(|(id, _)| *id == run_id));
        assert!(caches.contains_key("/doc/p"), "text cache recorded for later deltas");
    }

    #[test]
    fn merge_replaces_same_id_nodes_appends_new_and_adopts_focus() {
        let mut ids = NodeIdMap::new();
        let root_id = ids.id_for("/win");
        let extra_id = ids.id_for("/extra");
        let mut full = TreeUpdate {
            nodes: vec![(root_id, Node::new(accesskit::Role::Window))],
            tree: Some(Tree::new(root_id)),
            tree_id: TreeId::ROOT,
            focus: root_id,
        };
        let mut replacement = Node::new(accesskit::Role::Window);
        replacement.set_label("fresh");
        let splice = TreeUpdate {
            nodes: vec![
                (root_id, replacement),
                (extra_id, Node::new(accesskit::Role::Cell)),
            ],
            tree: None,
            tree_id: TreeId::ROOT,
            focus: extra_id,
        };

        merge_update(&mut full, splice);

        assert_eq!(full.nodes.len(), 2);
        assert_eq!(full.nodes[0].1.label(), Some("fresh".into()));
        assert_eq!(full.nodes[1].0, extra_id);
        assert_eq!(full.focus, extra_id);
        assert!(full.tree.is_some(), "merge never clears the full update's tree");
    }

    #[test]
    fn consumer_applies_spliced_cell_focus() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/table".into()];
        let table = leaf("/table", Role::Table, "grid");
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        let full = build_window_update(&[root, table], &mut ids, &mut caches);
        let mut tree = accesskit_consumer::Tree::new(full, false);

        let mut fresh_table = leaf("/table", Role::Table, "grid");
        fresh_table.children = vec!["/table/cell".into()];
        let cell = leaf("/table/cell", Role::TableCell, "A1");
        let known: HashSet<String> = ["/win".to_owned(), "/table".to_owned()].into();
        let result = splice_chain_update(
            &[fresh_table, cell],
            &[],
            &known,
            &mut ids,
            &mut caches,
        )
        .expect("chain splices");
        let cell_id = ids.get("/table/cell").unwrap();

        tree.update_and_process_changes(result.update, &mut NoOpChanges);

        let state = tree.state();
        let cell_node = state
            .node_by_tree_local_id(cell_id, accesskit::TreeId::ROOT)
            .expect("spliced cell present in consumer tree");
        assert_eq!(state.focus_id_in_tree(), cell_node.id());
        assert_eq!(cell_node.role(), accesskit::Role::Cell);
    }
```

Shared no-op handler for consumer tests (top of the tests mod):

```rust
    struct NoOpChanges;

    impl accesskit_consumer::ChangeHandler for NoOpChanges {
        fn node_added(&mut self, _: &accesskit_consumer::Node) {}
        fn node_updated(&mut self, _: &accesskit_consumer::Node, _: &accesskit_consumer::Node) {}
        fn focus_moved(
            &mut self,
            _: Option<&accesskit_consumer::Node>,
            _: Option<&accesskit_consumer::Node>,
        ) {}
        fn node_removed(&mut self, _: &accesskit_consumer::Node) {}
    }
```

(`Node::children()`, `label()`, `Tree`, `TreeId`, `TreeUpdate` are already imported at mapping.rs:5. If `focus_id_in_tree` turns out to live elsewhere than `State`, use the accessor adjacent to `focus_id()` in accesskit_consumer 0.38 tree.rs:532 — verified present in the vendored source.)

- [ ] **Step 2: Run tests, verify they fail to compile** (the functions don't exist yet). Expected: `cannot find function `splice_chain_update``.

- [ ] **Step 3: Implement** (mapping.rs, after `focus_update`):

```rust
/// The element children each walked node contributes to the emitted tree —
/// [`MirrorNode::children`] filtered to the walked set, the same filter
/// `build_node` applies. Keyed by AT-SPI path.
pub fn emitted_children(nodes: &[MirrorNode]) -> HashMap<String, Vec<String>> {
    let walked: HashSet<&str> = nodes.iter().map(|node| node.path.as_str()).collect();
    nodes
        .iter()
        .map(|node| {
            let children = node
                .children
                .iter()
                .filter(|child| walked.contains(child.as_str()))
                .cloned()
                .collect();
            (node.path.clone(), children)
        })
        .collect()
}

/// A spliced chain turned into a partial update plus the bookkeeping the
/// caller folds into its per-window children map.
pub struct SpliceResult {
    pub update: TreeUpdate,
    /// The element children each chain node was emitted with, by path.
    pub children: Vec<(String, Vec<String>)>,
}

/// Splices a freshly read ancestor chain into an existing window tree.
/// `chain[0]` is the already-known ancestor's fresh read, each following node
/// a child of its predecessor, the last the new focus target. The ancestor is
/// emitted with `ancestor_children` (the client tree's current children) plus
/// the chain child appended; interior nodes keep only children in `known` or
/// the chain, so a lazy grid's huge fresh child list can neither bloat nor
/// orphan the client tree. Returns a partial update (`tree: None`) whose
/// focus is the descendant, or `None` for a chain shorter than two nodes.
pub fn splice_chain_update(
    chain: &[MirrorNode],
    ancestor_children: &[String],
    known: &HashSet<String>,
    ids: &mut NodeIdMap,
    text_caches: &mut HashMap<String, TextNodeCache>,
) -> Option<SpliceResult> {
    if chain.len() < 2 {
        return None;
    }
    let chain_paths: HashSet<&str> = chain.iter().map(|node| node.path.as_str()).collect();
    let mut per_node_children: Vec<Vec<String>> = Vec::with_capacity(chain.len());
    for (index, node) in chain.iter().enumerate() {
        let mut children: Vec<String> = if index == 0 {
            ancestor_children.to_vec()
        } else {
            node.children
                .iter()
                .filter(|child| known.contains(*child) || chain_paths.contains(child.as_str()))
                .cloned()
                .collect()
        };
        if let Some(next) = chain.get(index + 1) {
            if !children.contains(&next.path) {
                children.push(next.path.clone());
            }
        }
        per_node_children.push(children);
    }
    let mut spliced: Vec<MirrorNode> = chain.to_vec();
    for (node, children) in spliced.iter_mut().zip(&per_node_children) {
        node.children = children.clone();
    }
    let mut walked: HashSet<&str> = known.iter().map(String::as_str).collect();
    walked.extend(spliced.iter().map(|node| node.path.as_str()));
    walked.extend(ancestor_children.iter().map(String::as_str));
    let mut nodes_out = Vec::new();
    let mut focus = None;
    for node in &spliced {
        let id = ids.id_for(&node.path);
        let built = build_node(node, id, ids, &walked);
        nodes_out.push((id, built.container));
        nodes_out.extend(built.runs);
        if let Some(cache) = built.cache {
            text_caches.insert(node.path.clone(), cache);
        }
        focus = Some(id);
    }
    let update = TreeUpdate {
        nodes: nodes_out,
        tree: None,
        tree_id: TreeId::ROOT,
        focus: focus?,
    };
    let children = spliced
        .iter()
        .map(|node| node.path.clone())
        .zip(per_node_children)
        .collect();
    Some(SpliceResult { update, children })
}

/// Merges a splice delta into a full-tree update: same-id nodes are replaced,
/// new ones appended, and the splice's focus wins. The full update's `tree`
/// is untouched.
pub fn merge_update(full: &mut TreeUpdate, splice: TreeUpdate) {
    for (id, node) in splice.nodes {
        match full.nodes.iter_mut().find(|(existing, _)| *existing == id) {
            Some(slot) => slot.1 = node,
            None => full.nodes.push((id, node)),
        }
    }
    full.focus = splice.focus;
}
```

- [ ] **Step 4: Run the full suite, verify green.** Expected: all pass (56 now).

- [ ] **Step 5: Commit**

```bash
git add crates/accesskit_remote_atspi/src/mapping.rs
git commit -m "atspi: pure chain-splice machinery for on-demand nodes"
```

---

### Task 4: wire on-demand active-descendant resolution

**Files:**
- Modify: `crates/accesskit_remote_atspi/src/mirror.rs` (chain reader)
- Modify: `crates/accesskit_remote_atspi/src/source.rs` (`WindowState`, handlers, tests)

**Interfaces:**
- Consumes: `read_node` (Task 2), `splice_chain_update`/`SpliceResult`/`emitted_children` (Task 3), `AccessibleProxy::parent() -> zbus::Result<ObjectRefOwned>` (verified in vendored atspi-proxies 0.14 accessible.rs:235), `ObjectRef::new_owned` (already used in source.rs tests).
- Produces:
  - `pub(crate) const MAX_SPLICE_HOPS: usize = 16;` and `pub(crate) async fn read_chain_to_known(conn: &AccessibilityConnection, descendant: &ObjectRefOwned, known: &HashSet<String>, max_hops: usize) -> Option<Vec<(MirrorNode, ObjectRefOwned)>>` (mirror.rs — Task 5 reuses both)
  - `WindowState.children: HashMap<String, Vec<String>>`
  - `fn handle_active_descendant(...) -> Option<Vec<SourceEvent>>` (None = unknown descendant, caller escalates)
  - `fn apply_spliced_chain(&mut self, index: usize, chain: &[(MirrorNode, ObjectRefOwned)]) -> Option<accesskit::TreeUpdate>` (Task 5 reuses)

- [ ] **Step 1: Write the failing tests** (source.rs tests mod):

Extend the `window_state` helper (:720) — add the `children` field so the compiler forces every construction site through it:

```rust
        let mut children = HashMap::new();
        if walked {
            children.insert(root_path.to_owned(), vec![node_path.to_owned()]);
            children.insert(node_path.to_owned(), Vec::new());
        }
        WindowState {
            id: WindowId(id),
            root,
            ids,
            objects,
            focus: node_id,
            text: HashMap::new(),
            children,
        }
```

Add helpers + tests:

```rust
    fn obj(sender: &'static str, path: &'static str) -> ObjectRefOwned {
        ObjectRef::new_owned(
            UniqueName::from_static_str_unchecked(sender),
            ObjectPath::from_static_str_unchecked(path),
        )
    }

    fn mirror_node(path: &str, role: atspi::Role, name: &str) -> crate::mapping::MirrorNode {
        crate::mapping::MirrorNode {
            path: path.to_owned(),
            role,
            name: name.to_owned(),
            focusable: false,
            focused: false,
            actionable: false,
            children: Vec::new(),
            text: None,
        }
    }

    #[test]
    fn active_descendant_absent_from_the_tree_escalates() {
        let win = window_state(1, ":1.1", "/win/1", "/win/1/item", true);
        let mut mirror = mirror_with(vec![win]);
        assert!(mirror.handle_active_descendant(":1.1", "/win/1/gone").is_none());
    }

    #[test]
    fn apply_spliced_chain_updates_objects_children_and_focus() {
        let win = window_state(1, ":1.1", "/win/1", "/win/1/table", true);
        let mut mirror = mirror_with(vec![win]);
        let mut fresh_table = mirror_node("/win/1/table", atspi::Role::Table, "grid");
        fresh_table.children = vec!["/win/1/table/cell".to_owned()];
        let chain = vec![
            (fresh_table, obj(":1.1", "/win/1/table")),
            (
                mirror_node("/win/1/table/cell", atspi::Role::TableCell, "A1"),
                obj(":1.1", "/win/1/table/cell"),
            ),
        ];

        let update = mirror.apply_spliced_chain(0, &chain).expect("splice applies");

        let state = &mirror.windows[0];
        let cell = state.ids.get("/win/1/table/cell").expect("cell id allocated");
        assert_eq!(update.focus, cell);
        assert!(update.tree.is_none());
        assert!(state.objects.contains_key(&cell), "action routing reaches the cell");
        assert_eq!(state.children["/win/1/table"], vec!["/win/1/table/cell".to_owned()]);
        assert_eq!(state.focus, cell);
    }

    #[test]
    fn apply_spliced_chain_twice_is_idempotent() {
        let win = window_state(1, ":1.1", "/win/1", "/win/1/table", true);
        let mut mirror = mirror_with(vec![win]);
        let chain = || {
            let mut fresh_table = mirror_node("/win/1/table", atspi::Role::Table, "grid");
            fresh_table.children = vec!["/win/1/table/cell".to_owned()];
            vec![
                (fresh_table, obj(":1.1", "/win/1/table")),
                (
                    mirror_node("/win/1/table/cell", atspi::Role::TableCell, "A1"),
                    obj(":1.1", "/win/1/table/cell"),
                ),
            ]
        };
        let first = mirror.apply_spliced_chain(0, &chain()).expect("splice applies");
        let second = mirror.apply_spliced_chain(0, &chain()).expect("re-splice applies");
        assert_eq!(first.focus, second.focus);
        assert_eq!(
            mirror.windows[0].children["/win/1/table"],
            vec!["/win/1/table/cell".to_owned()],
            "no duplicate child entries"
        );
    }

    #[test]
    fn apply_spliced_chain_without_an_anchored_ancestor_applies_nothing() {
        let win = window_state(1, ":1.1", "/win/1", "/win/1/table", true);
        let mut mirror = mirror_with(vec![win]);
        let chain = vec![
            (mirror_node("/elsewhere", atspi::Role::Table, ""), obj(":1.1", "/elsewhere")),
            (
                mirror_node("/elsewhere/cell", atspi::Role::TableCell, ""),
                obj(":1.1", "/elsewhere/cell"),
            ),
        ];
        assert!(mirror.apply_spliced_chain(0, &chain).is_none());
    }
```

Adapt the existing tests to the `Option` return: in `active_descendant_emits_a_focus_only_delta_and_window_focus` and `active_descendant_in_the_focused_window_dedups_the_window_focus`, change `let out = mirror.handle_active_descendant(...)` to `let out = mirror.handle_active_descendant(...).expect("descendant resolves");` and delete the old `active_descendant_absent_from_the_tree_emits_nothing` (replaced by `..._escalates`).

- [ ] **Step 2: Run tests, verify compile failure** (no `children` field, no `apply_spliced_chain`). Expected: struct-field and method-not-found errors.

- [ ] **Step 3: Implement.**

**mirror.rs** — add `HashSet` to the `std::collections` import; add below `read_node`:

```rust
/// Bounds the parent climb from an unwalked descendant to a known ancestor.
pub(crate) const MAX_SPLICE_HOPS: usize = 16;

/// Reads `descendant`, then climbs `Accessible.Parent` until a path in
/// `known` is reached, reading each object on the way. Returns the chain
/// ancestor-first (`chain[0]` is the known ancestor's fresh read, the last
/// element the descendant), or `None` when a read fails, a parent is null,
/// or the hop budget runs out.
pub(crate) async fn read_chain_to_known(
    conn: &AccessibilityConnection,
    descendant: &ObjectRefOwned,
    known: &HashSet<String>,
    max_hops: usize,
) -> Option<Vec<(MirrorNode, ObjectRefOwned)>> {
    let zconn = conn.connection();
    let mut chain: Vec<(MirrorNode, ObjectRefOwned)> = Vec::new();
    let mut current = descendant.clone();
    for _ in 0..max_hops {
        let (node, _) = read_node(zconn, &current).await?;
        let reached_known = known.contains(&node.path);
        chain.push((node, current.clone()));
        if reached_known {
            chain.reverse();
            return Some(chain);
        }
        let proxy = current.as_accessible_proxy(zconn).await.ok()?;
        let parent = proxy.parent().await.ok()?;
        if parent.is_null() {
            return None;
        }
        current = parent;
    }
    None
}
```

**source.rs** — extend the `crate::mapping` import with `emitted_children` and `splice_chain_update`; add to `WindowState`:

```rust
    /// The element children each walked node was emitted with, by path — the
    /// client tree's current structure, consulted when splicing new nodes in.
    children: HashMap<String, Vec<String>>,
```

Fill it in `add_discovered` (insert `children: emitted_children(&nodes),` into the `WindowState` literal at :323) and in `rewalk` (add `state.children = emitted_children(&nodes);` next to the `index_objects` line at :607).

Change `handle_active_descendant` (:518) — `Some` when resolved, `None` signals the caller to splice:

```rust
    fn handle_active_descendant(
        &mut self,
        sender: &str,
        descendant_path: &str,
    ) -> Option<Vec<SourceEvent>> {
        resolve_focus_target(&self.windows, sender, descendant_path)
            .map(|(window, node)| self.emit_node_focus(window, node))
    }
```

Update its caller in `handle_atspi_event` (:401-404):

```rust
        if let Event::Object(ObjectEvents::ActiveDescendantChanged(ev)) = &event {
            let sender = event.sender();
            let path = ev.descendant.path_as_str();
            return match self.handle_active_descendant(sender.as_str(), path) {
                Some(out) => out,
                None => self.splice_active_descendant(conn, sender.as_str(), path).await,
            };
        }
```

Add the two new methods to `impl Mirror` (after `emit_node_focus`):

```rust
    /// Resolves an active descendant missing from the walked tree by reading
    /// it and its ancestors up to a known node directly off the bus, splicing
    /// the chain into the owning window, and focusing it. Emits nothing when
    /// the chain cannot be read or no tracked window anchors it. The event
    /// sender (not the event body's embedded name) addresses the objects,
    /// matching `resolve_focus_target`'s sender pinning.
    async fn splice_active_descendant(
        &mut self,
        conn: &AccessibilityConnection,
        sender: &str,
        descendant_path: &str,
    ) -> Vec<SourceEvent> {
        let Ok(name) = UniqueName::try_from(sender.to_owned()) else {
            return Vec::new();
        };
        let Ok(path) = ObjectPath::try_from(descendant_path.to_owned()) else {
            return Vec::new();
        };
        let descendant = ObjectRef::new_owned(name, path);
        let candidates: Vec<usize> = self
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.root.name().is_some_and(|n| n.as_str() == sender))
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }
        let known: HashSet<String> = candidates
            .iter()
            .flat_map(|&index| self.windows[index].children.keys().cloned())
            .collect();
        let Some(chain) =
            mirror::read_chain_to_known(conn, &descendant, &known, mirror::MAX_SPLICE_HOPS).await
        else {
            return Vec::new();
        };
        let anchor = chain[0].0.path.clone();
        let Some(index) = candidates
            .into_iter()
            .find(|&index| self.windows[index].children.contains_key(&anchor))
        else {
            return Vec::new();
        };
        let Some(update) = self.apply_spliced_chain(index, &chain) else {
            return Vec::new();
        };
        let window = self.windows[index].id;
        let mut out = vec![SourceEvent::TreeUpdate { window, update }];
        if let Some(change) = self.focus.focus(window) {
            out.push(SourceEvent::FocusChanged(change));
        }
        out
    }

    /// Applies a freshly read chain to the window at `index`: allocates ids,
    /// builds the splice update, and folds the chain into `objects`,
    /// `children`, and `focus`. `None` when the chain's first node is not a
    /// known ancestor of this window.
    fn apply_spliced_chain(
        &mut self,
        index: usize,
        chain: &[(crate::mapping::MirrorNode, ObjectRefOwned)],
    ) -> Option<accesskit::TreeUpdate> {
        let nodes: Vec<crate::mapping::MirrorNode> =
            chain.iter().map(|(node, _)| node.clone()).collect();
        let state = &mut self.windows[index];
        let ancestor_children = state.children.get(nodes.first()?.path.as_str())?.clone();
        let known: HashSet<String> = state.children.keys().cloned().collect();
        let result = splice_chain_update(
            &nodes,
            &ancestor_children,
            &known,
            &mut state.ids,
            &mut state.text,
        )?;
        for (node, object) in chain {
            if let Some(id) = state.ids.get(&node.path) {
                state.objects.insert(id, object.clone());
            }
        }
        for (path, children) in result.children {
            state.children.insert(path, children);
        }
        state.focus = result.update.focus;
        Some(result.update)
    }
```

(`UniqueName`/`ObjectPath`/`ObjectRef` move from test-only imports to the module level: `use atspi::object_ref::ObjectRef; use atspi::zbus::names::UniqueName; use atspi::zbus::zvariant::ObjectPath;`.)

- [ ] **Step 4: Run the full suite, verify green.** Expected: all pass (adapted + 4 new; ~60).

- [ ] **Step 5: Live-verify on Calc, regression-check gnome-text-editor**

Calc: `wsl -e bash -lc 'busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true; SAL_USE_VCLPLUGIN=gtk3 LIBGL_ALWAYS_SOFTWARE=1 setsid soffice --calc --norestore >/tmp/lo.log 2>&1 </dev/null &'`, sleep ~12s, run the `window_lifecycle` example in the background, then drive cell navigation with `xdotool` — arrow keys first; if the XTEST key regression (workflow note 2026-07-24) bites, fall back to `xdotool` clicks on different cells (clicks still deliver, and also fire `active-descendant-changed`).
Expected: first move to a new cell → `TreeUpdate n (k nodes)` with `k ≥ 2` (the splice); moving back → `TreeUpdate n (0 nodes)` (the fast path now resolves it). Then the gnome-text-editor pass: `windowfocus` toggles and editing behave exactly as before (focus-only deltas, minimal text deltas).

- [ ] **Step 6: Commit**

```bash
git add crates/accesskit_remote_atspi/src/mirror.rs crates/accesskit_remote_atspi/src/source.rs
git commit -m "atspi: splice unwalked active descendants on demand"
```

---

### Task 5: rewalk-vs-spliced-focus guard

**The hazard:** `rewalk` rebuilds `objects`/`children` from the walk, so a spliced cell vanishes and `build_window_update` derives focus from walked `State::Focused` flags (root fallback) — not consumer-fatal, but focus visibly jumps off the cell on every debounced re-walk (Calc re-walks constantly while editing). Retaining the stale focus id instead WOULD be fatal (later `refresh_text` deltas stamp `window_state.focus` into partial updates). **Decision: re-splice the focused chain after the walk; fall back to the walk's own focus when the object genuinely died.**

**Files:**
- Modify: `crates/accesskit_remote_atspi/src/source.rs` (`rewalk`)
- Modify: `crates/accesskit_remote_atspi/src/mapping.rs` (consumer sequence test)

**Interfaces:**
- Consumes: `read_chain_to_known`/`MAX_SPLICE_HOPS`, `apply_spliced_chain`, `merge_update` (extend the `crate::mapping` import).

- [ ] **Step 1: Write the failing-to-compile pin** (mapping.rs tests — pure sequence proof that the merged update keeps the consumer sane; it exercises `merge_update` + `splice_chain_update` together, green once Task 5's step 3 compiles since the pure pieces landed in Task 3 — its value is pinning the exact rewalk sequence):

```rust
    #[test]
    fn consumer_survives_rewalk_that_drops_the_spliced_focus() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/table".into()];
        let table = leaf("/table", Role::Table, "grid");
        let walk_nodes = vec![root, table];
        let known: HashSet<String> = ["/win".to_owned(), "/table".to_owned()].into();
        let chain = || {
            let mut fresh_table = leaf("/table", Role::Table, "grid");
            fresh_table.children = vec!["/table/cell".into()];
            vec![fresh_table, leaf("/table/cell", Role::TableCell, "A1")]
        };

        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        let full_a = build_window_update(&walk_nodes, &mut ids, &mut caches);
        let mut tree = accesskit_consumer::Tree::new(full_a, false);

        let splice = splice_chain_update(&chain(), &[], &known, &mut ids, &mut caches)
            .expect("chain splices");
        let cell_id = ids.get("/table/cell").unwrap();
        assert_eq!(splice.update.focus, cell_id);
        tree.update_and_process_changes(splice.update, &mut NoOpChanges);

        let mut full_b = build_window_update(&walk_nodes, &mut ids, &mut caches);
        assert_ne!(full_b.focus, cell_id, "the walk alone reverts focus");
        let resplice = splice_chain_update(&chain(), &[], &known, &mut ids, &mut caches)
            .expect("re-splice applies");
        merge_update(&mut full_b, resplice.update);
        tree.update_and_process_changes(full_b, &mut NoOpChanges);

        let state = tree.state();
        let cell_node = state
            .node_by_tree_local_id(cell_id, accesskit::TreeId::ROOT)
            .expect("cell survives the merged rewalk");
        assert_eq!(state.focus_id_in_tree(), cell_node.id());
    }
```

Run it; expected: PASS already (pure pieces exist) — it is the sequence pin. The behavioral change itself is bus-side and verified live in Step 3-4.

- [ ] **Step 2: Implement the guard** — replace `rewalk` (source.rs:594-610):

```rust
    /// Re-walks one window and rebuilds its tree, reusing its stable id map.
    /// A focused node the fresh walk cannot see (a spliced lazy cell) is
    /// re-spliced into the update; when that fails, the walk's own focus
    /// stands.
    async fn rewalk(
        &mut self,
        conn: &AccessibilityConnection,
        window: WindowId,
    ) -> Option<SourceEvent> {
        let index = self.windows.iter().position(|w| w.id == window)?;
        let root = self.windows[index].root.clone();
        let prev_focus_obj = {
            let state = &self.windows[index];
            state.objects.get(&state.focus).cloned()
        };
        let (nodes, objects_by_path) = mirror::walk_window(conn, &root).await.ok()?;
        if nodes.is_empty() {
            return None;
        }
        let state = &mut self.windows[index];
        let mut update = build_window_update(&nodes, &mut state.ids, &mut state.text);
        state.objects = index_objects(&nodes, &state.ids, &objects_by_path);
        state.children = emitted_children(&nodes);
        state.focus = update.focus;
        if let Some(prev) = prev_focus_obj {
            if !objects_by_path.contains_key(prev.path_as_str()) {
                let known: HashSet<String> =
                    self.windows[index].children.keys().cloned().collect();
                if let Some(chain) =
                    mirror::read_chain_to_known(conn, &prev, &known, mirror::MAX_SPLICE_HOPS).await
                {
                    if let Some(splice) = self.apply_spliced_chain(index, &chain) {
                        merge_update(&mut update, splice);
                    }
                }
            }
        }
        Some(SourceEvent::TreeUpdate { window, update })
    }
```

(`apply_spliced_chain` already sets `state.focus` from the splice, so the state and the merged update agree.)

- [ ] **Step 3: Run the full suite, verify green.**

- [ ] **Step 4: Live-verify on Calc** — navigate to a cell (splice observed in `window_lifecycle` output), then force a debounced re-walk (type into the cell, or click a toolbar button → `children-changed` burst): the following full `TreeUpdate` must keep focus on the cell (the printed update's focus id unchanged), not jump to the root. Optional deeper pass: daemon + Windows `viewer` E2E, confirm no consumer panic while navigating + editing.

- [ ] **Step 5: Commit**

```bash
git add crates/accesskit_remote_atspi/src/source.rs crates/accesskit_remote_atspi/src/mapping.rs
git commit -m "atspi: re-splice a focused node the rewalk cannot see"
```

---

### Task 6: container bounds via `Component.GetExtents`

Scope decision (accepted risk): read extents for **every** node exposing `Interface::Component` — the interface set is already fetched, UIA clients want rects everywhere, and a uniform rule beats a role list. Cost ≈ +1 bus call/node/walk (~+20% on the ~5 calls/node walk); measure against Task 1's Writer baseline. The fallback knob (gate on text-bearing + window roles) is a one-line filter if measurement demands it — do not build it speculatively.

**Files:**
- Modify: `crates/accesskit_remote_atspi/src/mapping.rs` (`MirrorNode`, `build_node`, tests)
- Modify: `crates/accesskit_remote_atspi/src/mirror.rs` (`read_node`, new extents reader)

**Interfaces:**
- Consumes: `ComponentProxy::get_extents(CoordType) -> zbus::Result<(i32,i32,i32,i32)>` (verified in vendored atspi-proxies 0.14 component.rs:34), the builder pattern `perform` already uses (mirror.rs:299-304).
- Produces: `MirrorNode.bounds: Option<CharExtent>` — Task 7 relies on this field name.

- [ ] **Step 1: Write the failing tests** (mapping.rs tests):

```rust
    #[test]
    fn container_bounds_become_node_bounds() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.bounds = Some(ext(10, 20, 200, 30));
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root], &mut ids, &mut HashMap::new());
        assert_eq!(
            update.nodes[0].1.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 20.0, x1: 210.0, y1: 50.0 })
        );
    }

    #[test]
    fn zero_area_container_bounds_are_dropped() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.bounds = Some(ext(10, 20, 0, 30));
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root], &mut ids, &mut HashMap::new());
        assert_eq!(update.nodes[0].1.bounds(), None);
    }
```

- [ ] **Step 2: Run tests, verify compile failure** (`bounds` field missing).

- [ ] **Step 3: Implement.**

**mapping.rs** — add to `MirrorNode` (after `text`):

```rust
    /// The object's own window-relative extents from `Component.GetExtents`;
    /// `None` when the interface is absent or the read failed.
    pub bounds: Option<CharExtent>,
```

Extend `CharExtent`'s doc comment: `/// One window-relative extent as AT-SPI reports it: a code point's, or a whole object's.` Add `bounds: None,` to every `MirrorNode` literal the compiler flags: the `leaf` helper (mapping.rs tests), `mirror_node` helper (source.rs tests), and `read_node` (mirror.rs — replaced next). In `build_node`, before the children handling:

```rust
    if let Some(bounds) = node.bounds {
        if bounds.width > 0 && bounds.height > 0 {
            container.set_bounds(accesskit::Rect {
                x0: bounds.x as f64,
                y0: bounds.y as f64,
                x1: (bounds.x + bounds.width) as f64,
                y1: (bounds.y + bounds.height) as f64,
            });
        }
    }
```

**mirror.rs** — in `read_node`, after the `text` read:

```rust
    let bounds = if interfaces
        .as_ref()
        .is_some_and(|set| set.contains(Interface::Component))
    {
        read_component_extents(zconn, obj).await
    } else {
        None
    };
```

…and `bounds,` in the `MirrorNode` literal. New reader below `read_char_extents`:

```rust
/// Reads an object's own window-relative extents off its `Component`
/// interface; `None` on any failure.
async fn read_component_extents(
    zconn: &atspi::zbus::Connection,
    obj: &ObjectRefOwned,
) -> Option<CharExtent> {
    let name: BusName = obj.name()?.clone().into();
    let path = obj.path().clone();
    let proxy = ComponentProxy::builder(zconn)
        .destination(name)
        .ok()?
        .path(path)
        .ok()?
        .build()
        .await
        .ok()?;
    let (x, y, width, height) = proxy.get_extents(CoordType::Window).await.ok()?;
    Some(CharExtent { x, y, width, height })
}
```

- [ ] **Step 4: Run the full suite, verify green.**

- [ ] **Step 5: Live-verify + measure** — `dump_tree` vs gnome-text-editor: buttons/labels now print container rects (plausible window-relative values); `time` the Writer enumeration (same doc state as Task 1's baseline) and compare. Record both numbers for Task 8. If the walk degrades badly (>2× baseline), flag to the user before proceeding — the fallback gate is a decision, not an auto-apply.

- [ ] **Step 6: Commit**

```bash
git add crates/accesskit_remote_atspi/src/mapping.rs crates/accesskit_remote_atspi/src/mirror.rs
git commit -m "atspi: read Component extents into container bounds"
```

---

### Task 7: empty-field caret anchor

**Files:**
- Modify: `crates/accesskit_remote_atspi/src/mapping.rs` (`build_text_runs` signature, `TextNodeCache`, `build_node`, `rebuild_text_node`, tests)

**Interfaces:**
- Consumes: `MirrorNode.bounds` (Task 6).
- Produces: `build_text_runs(parent_path, text, extents, container_bounds: Option<CharExtent>, ids)` — 5-arg form; `TextNodeCache.container_bounds: Option<CharExtent>`.

- [ ] **Step 1: Write the failing tests.** Rename `empty_text_has_no_geometry` (:1253) → `empty_text_without_container_bounds_has_no_geometry` (same body once the 5th arg `None` is added). New tests:

```rust
    #[test]
    fn empty_text_anchors_caret_to_container() {
        let extents: [CharExtent; 0] = [];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs(
            "/n",
            "",
            Some(&extents),
            Some(ext(10, 20, 200, 30)),
            &mut ids,
        );

        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].1.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 20.0, x1: 10.0, y1: 50.0 })
        );
        assert_eq!(runs[0].1.character_positions(), Some(&[][..]));
        assert_eq!(runs[0].1.character_widths(), Some(&[][..]));
        assert_eq!(runs[0].1.text_direction(), Some(accesskit::TextDirection::LeftToRight));
    }

    #[test]
    fn nonempty_text_without_extents_ignores_container() {
        let mut ids = NodeIdMap::new();
        let (runs, _) =
            build_text_runs("/n", "ab", None, Some(ext(10, 20, 200, 30)), &mut ids);
        assert_eq!(runs[0].1.bounds(), None, "a synthetic anchor never mixes with real text");
    }

    #[test]
    fn clearing_text_keeps_the_container_caret_anchor() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/entry".into()];
        let mut entry = leaf("/entry", Role::Entry, "");
        entry.bounds = Some(ext(10, 20, 200, 30));
        entry.text = Some(TextState {
            text: "a".into(),
            caret: Some(1),
            selection: None,
            extents: Some(vec![ext(10, 20, 8, 16)]),
        });
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        build_window_update(&[root, entry], &mut ids, &mut caches);
        let cache = caches.get_mut("/entry").unwrap();

        let cleared = TextState {
            text: String::new(),
            caret: Some(0),
            selection: None,
            extents: Some(Vec::new()),
        };
        let changed = rebuild_text_node(cache, "/entry", &cleared, &mut ids);
        let run_id = ids.get("/entry#run0").unwrap();
        let (_, run) = changed
            .iter()
            .find(|(id, _)| *id == run_id)
            .expect("emptied run re-emitted");
        assert_eq!(
            run.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 20.0, x1: 10.0, y1: 50.0 })
        );
    }

    #[test]
    fn consumer_exposes_caret_anchor_on_empty_field() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/entry".into()];
        let mut entry = leaf("/entry", Role::Entry, "");
        entry.bounds = Some(ext(10, 20, 200, 30));
        entry.text = Some(TextState {
            text: String::new(),
            caret: Some(0),
            selection: None,
            extents: Some(Vec::new()),
        });
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root, entry], &mut ids, &mut HashMap::new());
        let run_id = ids.get("/entry#run0").unwrap();

        let tree = accesskit_consumer::Tree::new(update, false);
        let state = tree.state();
        let run = state
            .node_by_tree_local_id(run_id, accesskit::TreeId::ROOT)
            .expect("empty run present");
        assert_eq!(
            run.bounding_box(),
            Some(accesskit::Rect { x0: 10.0, y0: 20.0, x1: 10.0, y1: 50.0 })
        );
    }
```

- [ ] **Step 2: Run tests, verify compile failure** (4-arg calls vs 5-arg signature) then, after mechanically adding `None` as the new 4th argument to every existing `build_text_runs` call in tests and non-test code, verify the three new behavior tests FAIL (no anchor logic yet).

- [ ] **Step 3: Implement.** `build_text_runs` — new parameter and anchor branch (doc comment gains: "An empty text's single run takes a zero-width rect at `container_bounds`' left edge when no extent can anchor it."):

```rust
pub fn build_text_runs(
    parent_path: &str,
    text: &str,
    extents: Option<&[CharExtent]>,
    container_bounds: Option<CharExtent>,
    ids: &mut NodeIdMap,
) -> (Vec<(NodeId, Node)>, Vec<TextRunLayout>) {
```

after the `synthesized` binding:

```rust
    let caret_anchor = if synthesized.is_none() && text.is_empty() {
        container_bounds
    } else {
        None
    };
```

and the geometry `if let` gains an else-arm:

```rust
        } else if let Some(anchor) = caret_anchor {
            let edge = anchor.x as f64;
            node.set_bounds(accesskit::Rect {
                x0: edge,
                y0: anchor.y as f64,
                x1: edge,
                y1: (anchor.y + anchor.height) as f64,
            });
            node.set_character_positions(Vec::new());
            node.set_character_widths(Vec::new());
            node.set_text_direction(accesskit::TextDirection::LeftToRight);
        }
```

`TextNodeCache` gains `pub container_bounds: Option<CharExtent>,`. `build_node` passes `node.bounds` as the new argument and sets `container_bounds: node.bounds,` in the cache literal. `rebuild_text_node` passes `cache.container_bounds`. Refresh decision (accepted): **no Component re-read on text events** — the cached rect is reused and refreshes on the next re-walk, the same staleness class as cached char extents on caret moves.

- [ ] **Step 4: Run the full suite, verify green.**

- [ ] **Step 5: Live-verify** — clean-slate gnome-text-editor (`rm -rf ~/.local/share/gnome-text-editor` first, then the usual launch): `dump_tree` shows the empty document's single run with a zero-width rect instead of no geometry. Then type a character and delete it (or use `caret_reflect`) to see the cleared-field delta carry the anchor.

- [ ] **Step 6: Commit**

```bash
git add crates/accesskit_remote_atspi/src/mapping.rs
git commit -m "atspi: anchor an empty field's caret run to its container bounds"
```

---

### Task 8: update `docs/next-steps.md`

**Files:**
- Modify: `docs/next-steps.md`

- [ ] **Step 1: Record what landed** — a new milestone bullet in "What works end to end" covering: Paragraph runs (LO Writer follow-up (i) done), on-demand active-descendant splice + rewalk re-splice guard (follow-up (ii) done; note the `objects`-equals-client-tree invariant and that splice bookkeeping keeps `children` consistent so previously spliced cells are never orphaned), container bounds + empty-field caret anchor (Remaining 5(d)'s tail: container/element bounds ✔, empty-field caret anchor ✔; still open: RTL direction). Include the measured Writer walk times (Task 1 and Task 6 data) and any live-verification caveats hit (xdotool keys vs clicks on Calc). Strike the two LibreOffice follow-ups in their section and update Remaining 5(d)'s parenthetical to leave only RTL.

- [ ] **Step 2: Commit**

```bash
git add docs/next-steps.md
git commit -m "Docs: paragraph runs, active-descendant splice, container bounds"
```

---

## Edge cases addressed

- **Rewalk prunes the spliced focus** → Task 5: re-splice, else the walk's focus stands; a stale focus id is never retained (would be consumer-fatal via `refresh_text`'s stamped focus).
- **`objects` invariant** (`objects` == the client tree, exactly): spliced entries inserted at splice time; on rewalk everything is rebuilt from the walk and only the re-spliced focused chain is re-added.
- **Second splice under the same ancestor** → `state.children` is updated on every splice, so a later splice's `ancestor_children` includes earlier cells — nothing gets orphaned, and `resolve_focus_target` never targets a dropped node.
- **Lazy grid's huge fresh child list** → the ancestor's emitted children come from the client tree's snapshot plus the one chain child; unknown fresh children are filtered out (tested).
- **Duplicate/racing splices** → `NodeIdMap::id_for` stability + append-if-absent makes re-splicing idempotent (tested at both layers).
- **Chain climb failures** → dead object, null parent, >16 hops, no anchoring window → emit nothing (today's drop behavior, no regression).
- **Cell content** → chain nodes go through the same role gates as the walk; `TableCell` is not a text role, so content arrives as `name` (revisit only if Calc cells expose Text content that names miss).
- **Empty-field anchor** → applied only when the text is empty and no extent exists; zero-area container rects dropped; anchor height = container height (approximate v1, flagged in docs).

## Risks / open items

1. **Writer geometry walk cost** (Task 1 measures; Task 6 re-measures): per-paragraph char-extent reads could make a text-heavy Writer walk slow. Accept-and-measure; a per-window geometry budget is the known mitigation if needed (not in scope).
2. **xdotool XTEST keys** may not reach Calc (2026-07-24 regression note) — click-based cell navigation is the fallback; if both fail, Task 4/5 rest on unit + consumer tests until the interactive RAIL pass.
3. **accesskit_consumer API drift**: `focus_id_in_tree`, `document_range().text()`, `bounding_box()` were verified in the vendored 0.38.0 source; if a name differs at compile time, the neighbors in tree.rs:532 / text.rs:1438 / node.rs:315 are the reference.
4. `handle_focus_change`'s unwalked fallback still full-rewalks; it could reuse the splice machinery later — noted, out of scope.

## Verification (end-to-end)

After Task 8: full suite green in WSL; then one combined live pass — Calc (gtk3): navigate cells → splice + focus-follow observed, edit a cell → debounced rewalk keeps cell focus; Writer (gtk3): `dump_tree` shows paragraph runs with geometry and container rects; gnome-text-editor: unchanged baseline behavior plus the empty-document caret anchor. Optional capstone: daemon `--atspi --vsock 4750` + Windows `viewer` + PowerShell 5.1 UIA read of a Calc cell's name after navigation (the workflow-notes recipe).
