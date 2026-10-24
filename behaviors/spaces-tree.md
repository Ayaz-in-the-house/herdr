# Spaces as a three-level tree — registered behaviors

Fork feature. The Spaces tab shows a repository, the checkouts under it, and the
remembered chats under each checkout — three levels, one meaning per control.

It exists because of a specific, measured failure. The Spaces tab had grown two
independent disclosures: "show this repository's other checkouts" and "show this
checkout's remembered chats" (`workspace-chats.md`). Both were drawn as the same
arrow, in the same leading gutter, one column apart, on the same row:

```
▾▾· herdr               +      <- two arrows, two different controls
   master
      spaces cekmece tas…      <- chats, floating, unattached
▾  · fix-clipboard      +      <- a child's arrow in the parent's column
```

Three things were wrong and none of them were fixable by styling:

- **One row, two disclosures.** No reader can tell which arrow folds the
  repository and which opens the chats when they are adjacent and identical.
- **`▸` meant three things** — group folded, drawer folded, and *this chat is
  the focused tab*. A glyph with three meanings carries none.
- **A grouped child drew its arrow in column 0**, the repository's column, so a
  checkout claimed to be a group.

The fix is structural rather than cosmetic: give the repository a row of its
own. Once the two disclosures live on different rows, they cannot be confused,
and the third meaning of `▸` can be handed back to the accent, which is how
everything else in this sidebar says "this is the one you are on".

Two ideas run through the family:

- **One gutter, one meaning.** A leading arrow means "open what is under me" and
  nothing else, at every depth. Depth is spent on indentation; state is spent on
  the dot; actions are spent on the trailing edge. No column does two jobs.
- **A header is not a workspace.** `GroupHeader` carries no `ws_idx` and is laid
  out into its own area vector. Nothing that resolves a workspace can reach it,
  which is what keeps a header press from folding, selecting, or switching to
  whichever workspace happens to share its row.

## Shape of the list

| ID | Behavior | Breaks if lost | Verified by |
|---|---|---|---|
| TP-TREE-01 | A repository with two or more checkouts gets a header row of its own, and that row owns the group's arrow. | Both disclosures return to one row one column apart, and no reader can tell which arrow does what. | `a_repository_with_two_checkouts_gets_a_header_row_of_its_own` |
| TP-TREE-02 | A repository with a single checkout gets no header. | Every workspace pays a header row, doubling the vertical cost of a dozen-plus list for a group that does not exist. | `a_lone_checkout_gets_no_header_row` |
| TP-TREE-03 | A folded group keeps its header and the checkout in use, and hides the rest. | Folding hides where you are standing, so the sidebar stops answering "which checkout am I in". | `collapsed_group_hides_inactive_children_but_keeps_active_visible` |
| TP-TREE-04 | Every checkout is a child of the header, the main one included. | The main checkout stays at the header's depth and its arrow lands back in the repository's column — the original collision. | `a_repository_with_two_checkouts_gets_a_header_row_of_its_own` · `parent_workspace_row_stays_clickable_when_grouped` |

## Layout

| ID | Behavior | Breaks if lost | Verified by |
|---|---|---|---|
| TP-TREE-05 | Header rows are laid out into their own vector, never into the workspace-indexed one. | A header press resolves as whichever workspace shares its position — the trap already paid for by the tab strip (TP-FTAB-ENTRY-05) and the chat drawer (TP-WSCHAT-17). | `parent_workspace_row_stays_clickable_when_grouped` |
| TP-TREE-06 | Every row kind is measured by one function, and the group gap falls only where a new top-level unit begins. | A drawn-but-uncounted row makes the list scroll past itself; a gap in the wrong place draws a separator inside a group instead of around it. | `the_scroll_metrics_count_the_header_row` · `space_row_gap_preserves_compact_worktree_children` |
| TP-TREE-07 | A header is laid out above the checkouts it introduces. | The header names a group that appears somewhere else in the list. | `parent_workspace_row_stays_clickable_when_grouped` |

## Drawing

| ID | Behavior | Breaks if lost | Verified by |
|---|---|---|---|
| TP-TREE-08 | No row ever carries two disclosure arrows side by side, at any width. | The original complaint returns. This is the machine-checked form of it. | `no_row_ever_carries_two_disclosure_arrows_side_by_side` |
| TP-TREE-09 | A drawer's rows hang off their checkout's arrow column on a vertical rule. | Chats float under the list unattached, reading as siblings of the checkouts rather than as their contents. | `the_tree_puts_the_repository_the_checkouts_and_the_chats_on_their_own_depths` |
| TP-TREE-10 | The repository owns column 0; a checkout's arrow sits one indent step in; the disclosure column is reserved on every row whether or not that row has an arrow. | A checkout claims to be a repository, and sibling names step in and out as drawers appear and disappear. | `the_tree_puts_the_repository_the_checkouts_and_the_chats_on_their_own_depths` |
| TP-TREE-11 | The selection accent lives on exactly one focus carrier: the active workspace row wears it with contrast text, except while the drawer shows the chat the active tab resumes — the accent then belongs to that chat row and the card steps back to a quiet active tone. | "Which workspace am I in" has two possible answers, or none — and with the carrier rule lost, the card and the chat wear the accent at once and the answer doubles again. | `the_active_workspace_row_wears_the_accent_and_a_chat_row_never_does` · `default_space_workspace_style_tracks_active_state` |
| TP-FOCUS-01 | With the active tab's chat visible in the open drawer, the accent descends to that chat row (contrast text, bold) and the workspace card gives it up. | The highlight answers at the wrong depth: the branch lights up while the chat being worked in stays plain — the exact miss the user reported. | `the_accent_descends_to_the_visible_active_chat_row` |
| TP-FOCUS-02 | With the drawer shut, the workspace card keeps the accent even though its chat is the real focus object. | The accent follows an invisible row and the sidebar shows no selection at all. | `the_card_keeps_the_accent_while_its_chat_is_hidden` |
| TP-FOCUS-04 | When the accent descends to the chat row, the card's trailing chrome (the "+" and the chat count) steps back to the text ink with the name; on the accent both keep the contrast ink. | Contrast ink stranded on the quiet active tone reads as a broken highlight and a wrong-colored "+" in the row's corner. | `the_plus_and_count_follow_the_card_off_the_accent`, `the_plus_keeps_the_contrast_ink_on_the_accent` |
| TP-FOCUS-03 | (mode-aware since chat_drawer_mode) In focused mode, arriving at a workspace opens its chat drawer; a fold made while standing in it holds until the user leaves and returns, and an empty history is never revealed onto its placeholder. all-active derives instead of revealing and manual never moves a drawer — TP-DRAWER-05/06 carry those halves. | The chat the accent descends to hides behind a fold the reader has to know to open — the "hidden drawer" the user reported. | `activating_a_workspace_reveals_its_chat_drawer`, `a_fold_made_inside_the_workspace_holds_until_reactivation`, `an_empty_history_is_not_revealed_on_activation` |
| TP-TREE-12 | Every checkout offers a "+" that starts a chat there, and a checkout with history says how many chats it remembers. The "+" is never bound to hover. | Starting a chat on a branch — the point of the row — needs a detour, or a pointer move repaints the sidebar and breaks the loop-saturation guard (TP-REPAINT-2B). | `every_checkout_offers_a_plus_and_reports_how_many_chats_it_remembers` |
| TP-TREE-13 | The tree lays out without overflow down to the minimum configurable width. | Depth is charged in columns, so the narrowest sidebar is exactly where a prefix wider than the row wraps or panics. | `the_narrowest_sidebar_still_draws_the_tree` |

## Pressing

| ID | Behavior | Breaks if lost | Verified by |
|---|---|---|---|
| TP-TREE-14 | Pressing a repository header folds or unfolds its group and does nothing else — it never switches workspace. | A header behaves as a workspace, so folding a group moves you somewhere you did not ask to go. | `a_repository_press_folds_its_group_and_a_checkout_arrow_opens_its_drawer` · `clicking_worktree_parent_chevron_toggles_group_only` |
| TP-TREE-15 | Pressing a checkout's own arrow opens that checkout's drawer, and leaves the group alone. | The two disclosures act on each other, which is the collision in behavioural form. | `a_repository_press_folds_its_group_and_a_checkout_arrow_opens_its_drawer` |

## Mobile

| ID | Behavior | Breaks if lost | Verified by |
|---|---|---|---|
| TP-TREE-19 | A reorder drag moves a worktree block only when it starts on a visible block root: a flat row, or the first workspace row after a group/project header — on the folded tree that is the checkout the user stands in, and the block always travels main-checkout-first. The drop seam after the last card is decided by the next workspace row, never by a trailing chat/bucket row. | Upstream's block drag reads `indented: false` as "root"; on the fork's header-rooted tree no member matches, so group drags silently do nothing, member drags move blocks they should not, and a trailing drawer swallows the End slot. | `dragging_worktree_parent_reorders_the_complete_group` · `dragging_collapsed_worktree_parent_still_moves_hidden_children` · `dragging_worktree_space_member_does_not_reorder_workspaces` |
| TP-TREE-18 | The mobile drawer *does* see the header row. **Reversed 2026-08-01.** | The old rule protected the flat switcher, which derived a workspace position arithmetically, so a header shifted every position after it. The drawer that replaced it derives every position from one row list that render, hit-testing, height and the cursor all read, so an extra row kind cannot desynchronise them — and without the header a reader on a phone cannot tell which repository a worktree belongs to (TP-MOB-60). | `the_mobile_drawer_sees_the_group_header` |
| TP-TREE-20 | A chat row spends ONE column on indent and none on a state marker: the chat glyph carries the wired state (accent plus bold when the chat is open in another tab, the row's accent when it is the focused one), and when the glyph column is switched off (`[spaces.icons] chat = ""`) the TITLE carries it instead, at no extra column. A daily row's title begins at indent+glyph+space; a container's row sits one `ROW_INDENT_STEP` further in. | The surface whose job is reading conversation titles gives four to six columns per row to chrome, and a title truncates that would have fitted; or, with the glyph column off, "open in another tab" is left with no carrier at all and the state becomes invisible. | `a_daily_chat_row_begins_where_its_glyph_and_one_space_put_it` · `a_container_chat_sits_one_step_in_from_a_daily_one` · `a_chat_open_in_another_tab_says_so_with_its_glyph` · `no_chat_row_spends_a_column_on_a_state_marker` · `a_chat_open_in_another_tab_says_so_in_its_title_when_the_glyph_is_off` · `the_title_leaves_the_state_to_the_glyph_whenever_there_is_one` |
| TP-TREE-21 | A drawer chat's glyph sits directly beside the rule its checkout hangs it from: rule, glyph, one space, title — the rule pays for no pad of its own, and it keeps the checkout's arrow column. | The drawer drifts away from the rule that says what contains it, or the rule moves out of the column TP-TREE-09 puts it in. | `a_drawer_chat_sits_beside_the_rule_it_hangs_from` |
| TP-TREE-22 | A block boundary is ruled in the blank line the layout ALREADY leaves (`row_gap`): three cells between two branch blocks, the body's width less a right margin between two module blocks, both edges left clear, and nothing at all where no blank line was left. The rule is painted only — no row, no area vector, no change to what a press resolves against. | A rule that reached both edges reads as a divider between two panels rather than a seam inside one tree; a rule that added a row would move every hit target below it and desync the phone drawer. | `a_branch_boundary_wears_a_short_rule_only_where_a_blank_line_was_left` · `a_module_boundary_wears_a_long_rule_that_still_stops_short_of_both_edges` |
| TP-TREE-23 | The disclosure spends ONE column. What follows a checkout's arrow is the state glyph, never a word, so the second drawn cell was breathing space against nothing; the row resumes in the very next column and every checkout name and drawer title below it begins one column further left. The toggle rect is that same single cell, which is the cell the hit test has always matched. | The surface reads as a tree pushed into its own gutter — on the reported tree a branch name began in column eleven of a thirty-column sidebar and its chats in column nine; or the rect and the drawn arrow disagree again and the press target stops meaning what the constant says. | `the_disclosure_is_drawn_in_one_column_and_the_row_resumes_in_the_next` · `expanded_sidebar_workspace_rows_show_state_before_name_without_numbers` |
| TP-TREE-24 | One tree level is worth ONE column. A project, a node under it, a module rule under that and a checkout under the module step in by one column each, and a drawer's rule keeps its checkout's arrow column. | Depth is charged in columns: at two columns a level, a four-level chain spent six columns on indentation before the first letter of a branch name, and on a thirty-column sidebar the module label, the branch name and the chat title were all truncated at once. | `each_level_of_the_tree_costs_one_column` |
| TP-TREE-25 | The workspace list keeps a one-column gutter at its left edge and nothing it draws enters it — no row, header, drawer rule or block seam. The gutter is charged once, on the list body rect, so every renderer and every hit target inherits it from the same measurement. Column budgets are declared in one place (`COLUMNS`, a `ColumnPlan`): gutter, step, disclosure, chat indent and drawer pad, with every x-origin derived from it. | A rule run up the left edge has no column of its own, and finding one later moves every row on the surface; or a renderer charges the gutter and a hit target does not, so what is pressed stops being what was drawn. Without the plan, a column budget change is a hunt through ten multiplications instead of one line. | `the_list_keeps_its_left_edge_clear_for_a_rail` |
| TP-FOCUS-SW-01 | The focus filter is opt-in: while it is off the tree is exactly the tree it has always been | A reader has to check a toggle before trusting the list, and every unfocused screen inherits a filter it never asked for | `an_unfocused_tree_is_the_whole_tree` |
| TP-FOCUS-SW-02 | Focused, the tree keeps the checkout being worked in (the selection while navigating, the active one otherwise) and every checkout running an agent — "running" borrowed from the agents panel so the two surfaces cannot disagree — and the modules left without members go quiet with them | The switch either hides the row you are standing on, or keeps headers over nothing, or the tree and the panel below it start naming different agents | `a_focused_tree_keeps_the_active_checkout_and_the_running_ones` · `a_focused_tree_drops_the_headers_left_without_members` |
| TP-FOCUS-SW-03 | A focus with no candidates shows the whole tree instead of an empty one | With nothing active and nothing running the filter blanks the sidebar, which reads as a broken panel rather than a focused one | `a_focus_with_nothing_to_show_shows_everything` |
| TP-FOCUS-SW-04 | The footer's filter slot belongs to the tab showing: "focus" on Spaces, "actives" on Projects, each flipping only its own filter, drawn accented while on and dim while off, and mouse chrome either way | Two controls for one idea, or one click flipping the other tab's filter behind the reader's back | `clicking_the_footer_toggle_on_spaces_flips_focus_only` · `clicking_the_footer_toggle_on_projects_leaves_focus_alone` · `the_spaces_footer_draws_its_focus_toggle_and_says_which_way_it_is_thrown` |
| TP-FOCUS-SW-05 | Focus is per display: narrowing one screen leaves every other screen's tree whole (config carries only where a new client starts) | One screen's reading choice reaches into another's — the multi-display rule this fork is built on | `focusing_one_display_leaves_the_other_tree_alone` |
| TP-FOCUS-NODE-01 | Focused, a container whose whole subtree has no visible checkout and no chat open in a tab is not drawn — header, chat rows and "empty module" line alike. | A module you are not working in keeps its header, its moved chats and its empty-module line in a tree whose whole job is to show what you *are* working in; the filter stops meaning anything and the surface reads as broken. | `focus_hides_a_module_whose_moved_chats_are_all_idle`, `a_filtered_module_does_not_claim_to_be_empty` |
| TP-FOCUS-NODE-02 | A container holding a chat that is open in a tab stays drawn under focus, whatever happened to its checkouts. The two kinds of member are judged separately and one live member is enough. | A filter that hides where you actually are is worse than no filter: the chat is on screen in a tab while the tree claims its container does not exist. This is TP-DAILY-06's rule on its second surface. | `focus_keeps_a_module_whose_moved_chat_is_running`, `a_filtered_checkout_does_not_silence_a_module_with_a_running_chat` |

## Header age badges

| ID | Behavior | Breaks if lost | Verified by |
|---|---|---|---|
| TP-CHATAGE-01 | Every project, node and bucket header wears the age of the newest chat anywhere under it — moved-in seat chats and member drawers alike, subtree-aggregated through the one edge-reader `tree_parent_of` — right-aligned, dim, short of the mouse chrome, and the name's truncation reserves room for it. | "En son hangisinde chat olmuş" was the request: without the badge the only way to know when a module was last worked in is opening every drawer under it. Aggregate through a second membership definition and the badge and the tree silently disagree about what is under a header — the #88 class. Skip the reservation and a long name paints under the badge (TP-DOTS-09's lesson on a new tenant). | `a_module_header_wears_the_age_of_its_newest_chat`, `a_project_header_shows_the_newest_of_its_subtree`, `a_moved_in_chat_ages_the_seat_it_sits_on` |
| TP-CHATAGE-02 | The badge reads the SAME clock the chat rows wear — `last_activity_time` (message > mtime > sighting, TP-DRAW-15) — never the sighting alone. | A restart re-sights every open chat; a badge on the sighting clock resets every header to "now" on every delivery — the exact defect family TP-DRAW-15 was built to end, reborn on a new surface. | `the_header_age_reads_the_message_clock_not_the_sighting` |
| TP-CHATAGE-03 | A header with no chat anywhere under it wears no badge at all. | An absent fact rendered as "now" claims work that never happened; rendered as a dash it is noise on every quiet module. Absence reads as absence. | `a_header_with_no_chats_wears_no_age` |
| TP-CHATAGE-04 | An age that does not fit beside its row's prefix is not drawn at all — on the drawer surface and the daily one alike. The age is right-aligned, so a row too narrow for both does not shorten it, it clips it. | A clipped age is a different number, not a shorter one: at the ten-column minimum "2957w" reached the screen as "957w", which reads as a fifty-year-old conversation rather than a fifty-seven-year-old one — a wrong fact stated confidently, on the surface whose whole job this session was to make the ages trustworthy. | `a_chat_row_wears_its_age_wherever_it_fits_and_never_half_of_one` |

## Cost

The sidebar renders server-side and reaches a remote session as the client's
ANSI diff, so wire cost tracks the number of changed cells and CPU cost tracks
the number of renders. Nothing here adds a periodic source of either: the tree
is static, differentiated by glyph, depth and colour, and every change to it is
caused by a keypress or a click. The one thing deliberately not done is binding
any visual state to hover, which would make a pointer move a repaint.

Sibling: `workspace-chats.md` (what the drawer remembers) ·
`space-partition.md` (which repository a checkout groups under) ·
`shared-surfaces.md` (per-display ownership).

- `node_is_live_in_focus` is the single gate for TP-FOCUS-NODE-01/02, and it sits
  on the `Job::Node` arm of the walk rather than at the three sites that push a
  node. Moving it back to the push sites looks equivalent and is not: declared
  roots reach the arm without ever passing a push site, so a root-level module
  would keep drawing while its children went quiet.
