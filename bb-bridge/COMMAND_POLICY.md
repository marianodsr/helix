# Command policy

What `editor/command` may run under each value of `[editor.bridge] commands`
(PROTOCOL.md). The policy is enforced inside Helix
(`helix-term/src/bridge/methods/command.rs`), so no client can skip it.

| Value | Allows |
|---|---|
| `safe` (default) | everything below marked allowed |
| `all` | everything |
| `none` | nothing: `editor/command` answers `unsupported` |

The policy in force is the stricter of the one Helix started with and the
current configuration: `:set`, `:config-reload` or an edited config file can
tighten it, never loosen it. Restart Helix to loosen it.

## What `safe` stops, and how

`safe` stops an agent from running a program outside bb's permission mode,
and from throwing away the user's work, by every route the bridge offers:

- **Typed** (`typed`): the name is resolved through Helix's own command map,
  aliases included (`sh`, `!`, `|`, `q!`, `bc!` …), and checked. The
  arguments are then parsed by Helix's own parser with a hook that refuses a
  `%sh{…}` expansion anywhere, including inside `"…"` strings and inside
  other expansions; the walk is the same as `expansion::expand`'s. Arguments
  are checked after expansion, so `:set %reg{a} …` is judged by what the
  register holds.
- **Static** (`static`): checked by name. Commands that wait for the next key
  are `unsupported` here (use `keys`).
- **Keys** (`keys`): every key is checked, before it is delivered, against
  whatever will receive it:
  - the editor: the key is looked up in the user's own key map without
    changing its state (pending keys, sticky modes, counts and `.` included)
    and every command it would run is checked as above: a key bound to
    `:sh make` or to a shell pipe is refused, a key bound to a macro is
    refused, `.` is refused when the insert it repeats ran a refused command.
    Characters typed in insert mode are text. Each `keys` request starts by
    dismissing a key sequence the user left half typed, so the check sees
    what the keys will do;
  - the `:` command line: Enter is checked as a `typed` line (and refused on
    an empty line, which would rerun the last command from history);
  - any other prompt: Enter is allowed only in a prompt this same request
    opened (so not in a shell prompt the user left open);
  - the command palette: only Escape;
  - a popup (hover, signature help, a menu): its own keys are allowed, and
    the key is also checked against the layer below, which it may reach;
  - anything else (for example the workspace trust dialog): only Escape.

  A refused key stops the sequence, Escape is sent, and the error says which
  key and how many were delivered before it.

Known limits:

- A key that is the argument of a command waiting for it (`f|`, `r!`, `fq`)
  is checked as if it were a command and may be refused.
- `safe` does not stop an agent from writing files (`:w path`, `:move`,
  `editor/applyEdits` with `save`) anywhere the user can. A written file can
  later make the user run something (a config file, a register used as a
  macro); stopping that is bb's permission mode's job for file writes.
- Code actions (`code_action`) and rename are applied as the language server
  sends them; a server may attach a command it then runs itself.

The tables below are generated from the code and checked by a test
(`cargo test -p helix-term policy_document_is_current`), which fails when
upstream adds a command nobody has classified.

<!-- generated: do not edit below this line -->
## Typed commands

| Command | Aliases | Under `safe` |
|---|---|---|
| `append-output` |  | refused: runs a shell command |
| `bb-annotations` |  | allowed |
| `bb-ask` |  | refused: sends a message to bb in the user's name |
| `bb-comment` |  | refused: sends a message to bb in the user's name |
| `bb-comments-send` |  | refused: sends a message to bb in the user's name |
| `bb-diff` |  | allowed |
| `bb-emit` |  | refused: makes bb act in the user's name |
| `bb-files` |  | refused: runs the file manager program when bb shows no tree |
| `bb-go` |  | allowed |
| `bb-locations` |  | allowed |
| `bb-next` |  | allowed |
| `bb-prev` |  | allowed |
| `bb-review` |  | allowed |
| `bb-send` |  | refused: sends a message to bb in the user's name |
| `bb-send!` |  | refused: sends a message to bb in the user's name |
| `bb-status` |  | allowed |
| `buffer-close` | `bc` `bclose` | allowed |
| `buffer-close!` | `bc!` `bclose!` | refused: discards unsaved changes |
| `buffer-close-all` | `bca` `bcloseall` | allowed |
| `buffer-close-all!` | `bca!` `bcloseall!` | refused: discards unsaved changes |
| `buffer-close-others` | `bco` `bcloseother` | allowed |
| `buffer-close-others!` | `bco!` `bcloseother!` | refused: discards unsaved changes |
| `buffer-next` | `bn` `bnext` | allowed |
| `buffer-previous` | `bp` `bprev` | allowed |
| `change-current-directory` | `cd` | allowed |
| `character-info` | `char` | allowed |
| `clear-register` |  | allowed |
| `clipboard-paste-after` |  | allowed |
| `clipboard-paste-before` |  | allowed |
| `clipboard-paste-replace` |  | allowed |
| `clipboard-yank` |  | allowed |
| `clipboard-yank-join` |  | allowed |
| `config-open` |  | allowed |
| `config-open-workspace` |  | allowed |
| `config-reload` |  | refused: reloads configuration from files an agent can write, which can name programs to run |
| `cquit` | `cq` | allowed |
| `cquit!` | `cq!` | refused: discards unsaved changes |
| `debug-eval` |  | refused: starts or drives the debugger, which runs programs |
| `debug-remote` | `dbg-tcp` | refused: starts or drives the debugger, which runs programs |
| `debug-start` | `dbg` | refused: starts or drives the debugger, which runs programs |
| `earlier` | `ear` | allowed |
| `echo` |  | allowed |
| `encoding` |  | allowed |
| `exit` | `x` `xit` | allowed |
| `exit!` | `x!` `xit!` | refused: overwrites a file that changed on disk since Helix read it |
| `format` | `fmt` | allowed |
| `get-option` | `get` | allowed |
| `goto` | `g` | allowed |
| `hsplit` | `hs` `sp` | allowed |
| `hsplit-new` | `hnew` | allowed |
| `indent-style` |  | allowed |
| `insert-output` |  | refused: runs a shell command |
| `later` | `lat` | allowed |
| `line-ending` |  | allowed |
| `log-open` |  | allowed |
| `lsp-restart` |  | allowed |
| `lsp-stop` |  | allowed |
| `lsp-workspace-command` |  | refused: runs a language server command, which can run programs |
| `move` | `mv` | allowed |
| `move!` | `mv!` | allowed |
| `new` | `n` | allowed |
| `noop` |  | allowed |
| `open` | `o` `edit` `e` | allowed |
| `pipe` | `\|` | refused: runs a shell command |
| `pipe-to` |  | refused: runs a shell command |
| `pop-directory` | `popd` | allowed |
| `primary-clipboard-paste-after` |  | allowed |
| `primary-clipboard-paste-before` |  | allowed |
| `primary-clipboard-paste-replace` |  | allowed |
| `primary-clipboard-yank` |  | allowed |
| `primary-clipboard-yank-join` |  | allowed |
| `push-directory` | `pushd` | allowed |
| `quit` | `q` | allowed |
| `quit!` | `q!` | refused: discards unsaved changes |
| `quit-all` | `qa` | allowed |
| `quit-all!` | `qa!` | refused: discards unsaved changes |
| `read` | `r` | allowed |
| `redraw` |  | allowed |
| `reflow` |  | allowed |
| `reload` | `rl` | refused while the buffer has unsaved changes |
| `reload-all` | `rla` | refused while any buffer has unsaved changes |
| `reset-diff-change` | `diffget` `diffg` | allowed |
| `run-shell-command` | `sh` `!` | refused: runs a shell command |
| `set-language` | `lang` | allowed |
| `set-option` | `set` | refused for the settings listed under Settings |
| `set-register` |  | allowed |
| `show-clipboard-provider` |  | allowed |
| `show-directory` | `pwd` | allowed |
| `show-directory-stack` |  | allowed |
| `sort` |  | allowed |
| `theme` |  | allowed |
| `toggle-option` | `toggle` | refused for the settings listed under Settings |
| `tree-sitter-highlight-name` |  | allowed |
| `tree-sitter-layers` |  | allowed |
| `tree-sitter-scopes` |  | allowed |
| `tree-sitter-subtree` | `ts-subtree` | allowed |
| `tutor` |  | allowed |
| `update` | `u` | allowed |
| `vsplit` | `vs` | allowed |
| `vsplit-new` | `vnew` | allowed |
| `workspace-exclude` |  | refused: reloads configuration from files an agent can write, which can name programs to run |
| `workspace-trust` |  | refused: lets the workspace's own configuration start programs, which is the user's decision |
| `workspace-untrust` |  | refused: reloads configuration from files an agent can write, which can name programs to run |
| `write` | `w` | allowed |
| `write!` | `w!` | refused: overwrites a file that changed on disk since Helix read it |
| `write-all` | `wa` | allowed |
| `write-all!` | `wa!` | refused: overwrites a file that changed on disk since Helix read it |
| `write-buffer-close` | `wbc` | allowed |
| `write-buffer-close!` | `wbc!` | refused: overwrites a file that changed on disk since Helix read it |
| `write-quit` | `wq` | allowed |
| `write-quit!` | `wq!` | refused: discards unsaved changes |
| `write-quit-all` | `wqa` `xa` | allowed |
| `write-quit-all!` | `wqa!` `xa!` | refused: discards unsaved changes |
| `yank-diagnostic` |  | allowed |
| `yank-join` |  | allowed |

Every typed command is also refused when an argument contains a `%sh{…}` expansion, at any depth.

## Settings

`:set-option` and `:toggle-option` are refused for these top-level settings and anything under them:

| Setting | Why |
|---|---|
| `shell` | names the program shell commands run |
| `clipboard-provider` | can name programs to run on every yank and paste |
| `terminal` | names the program the debugger runs in |
| `workspace-trust` | decides which workspaces may start programs |
| `bridge` | holds this command policy |

## Static commands

| Command | Under `safe` |
|---|---|
| `add_newline_above` | allowed |
| `add_newline_below` | allowed |
| `align_selections` | allowed |
| `align_view_bottom` | allowed |
| `align_view_center` | allowed |
| `align_view_middle` | allowed |
| `align_view_top` | allowed |
| `append_char_interactive` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `append_mode` | allowed |
| `buffer_picker` | allowed |
| `change_selection` | allowed |
| `change_selection_noyank` | allowed |
| `changed_file_picker` | allowed |
| `code_action` | allowed |
| `collapse_selection` | allowed |
| `command_mode` | allowed |
| `command_palette` | refused: runs any command, out of reach of the policy |
| `commit_undo_checkpoint` | allowed |
| `completion` | allowed |
| `copy_between_registers` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `copy_selection_on_next_line` | allowed |
| `copy_selection_on_prev_line` | allowed |
| `dap_continue` | refused: starts or drives the debugger, which runs programs |
| `dap_disable_exceptions` | refused: starts or drives the debugger, which runs programs |
| `dap_edit_condition` | refused: starts or drives the debugger, which runs programs |
| `dap_edit_log` | refused: starts or drives the debugger, which runs programs |
| `dap_enable_exceptions` | refused: starts or drives the debugger, which runs programs |
| `dap_launch` | refused: starts or drives the debugger, which runs programs |
| `dap_next` | refused: starts or drives the debugger, which runs programs |
| `dap_pause` | refused: starts or drives the debugger, which runs programs |
| `dap_restart` | refused: starts or drives the debugger, which runs programs |
| `dap_step_in` | refused: starts or drives the debugger, which runs programs |
| `dap_step_out` | refused: starts or drives the debugger, which runs programs |
| `dap_switch_stack_frame` | refused: starts or drives the debugger, which runs programs |
| `dap_switch_thread` | refused: starts or drives the debugger, which runs programs |
| `dap_terminate` | refused: starts or drives the debugger, which runs programs |
| `dap_toggle_breakpoint` | refused: starts or drives the debugger, which runs programs |
| `dap_variables` | refused: starts or drives the debugger, which runs programs |
| `decrement` | allowed |
| `delete_char_backward` | allowed |
| `delete_char_forward` | allowed |
| `delete_selection` | allowed |
| `delete_selection_noyank` | allowed |
| `delete_word_backward` | allowed |
| `delete_word_forward` | allowed |
| `diagnostics_picker` | allowed |
| `earlier` | allowed |
| `ensure_selections_forward` | allowed |
| `exit_select_mode` | allowed |
| `expand_selection` | allowed |
| `extend_char_left` | allowed |
| `extend_char_right` | allowed |
| `extend_line` | allowed |
| `extend_line_above` | allowed |
| `extend_line_below` | allowed |
| `extend_line_down` | allowed |
| `extend_line_up` | allowed |
| `extend_next_char` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `extend_next_long_word_end` | allowed |
| `extend_next_long_word_start` | allowed |
| `extend_next_sub_word_end` | allowed |
| `extend_next_sub_word_start` | allowed |
| `extend_next_word_end` | allowed |
| `extend_next_word_start` | allowed |
| `extend_parent_node_end` | allowed |
| `extend_parent_node_start` | allowed |
| `extend_prev_char` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `extend_prev_long_word_end` | allowed |
| `extend_prev_long_word_start` | allowed |
| `extend_prev_sub_word_end` | allowed |
| `extend_prev_sub_word_start` | allowed |
| `extend_prev_word_end` | allowed |
| `extend_prev_word_start` | allowed |
| `extend_search_next` | allowed |
| `extend_search_prev` | allowed |
| `extend_till_char` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `extend_till_prev_char` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `extend_to_column` | allowed |
| `extend_to_file_end` | allowed |
| `extend_to_file_start` | allowed |
| `extend_to_first_nonwhitespace` | allowed |
| `extend_to_last_line` | allowed |
| `extend_to_line_bounds` | allowed |
| `extend_to_line_end` | allowed |
| `extend_to_line_end_newline` | allowed |
| `extend_to_line_start` | allowed |
| `extend_to_word` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `extend_visual_line_down` | allowed |
| `extend_visual_line_up` | allowed |
| `file_explorer` | allowed |
| `file_explorer_in_current_buffer_directory` | allowed |
| `file_explorer_in_current_directory` | allowed |
| `file_picker` | allowed |
| `file_picker_in_current_buffer_directory` | allowed |
| `file_picker_in_current_directory` | allowed |
| `find_next_char` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `find_prev_char` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `find_till_char` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `flip_selections` | allowed |
| `format_selections` | allowed |
| `global_search` | allowed |
| `goto_column` | allowed |
| `goto_declaration` | allowed |
| `goto_definition` | allowed |
| `goto_file` | allowed |
| `goto_file_end` | allowed |
| `goto_file_hsplit` | allowed |
| `goto_file_start` | allowed |
| `goto_file_vsplit` | allowed |
| `goto_first_change` | allowed |
| `goto_first_diag` | allowed |
| `goto_first_nonwhitespace` | allowed |
| `goto_implementation` | allowed |
| `goto_last_accessed_file` | allowed |
| `goto_last_change` | allowed |
| `goto_last_diag` | allowed |
| `goto_last_line` | allowed |
| `goto_last_modification` | allowed |
| `goto_last_modified_file` | allowed |
| `goto_line` | allowed |
| `goto_line_end` | allowed |
| `goto_line_end_newline` | allowed |
| `goto_line_start` | allowed |
| `goto_next_buffer` | allowed |
| `goto_next_change` | allowed |
| `goto_next_class` | allowed |
| `goto_next_comment` | allowed |
| `goto_next_diag` | allowed |
| `goto_next_entry` | allowed |
| `goto_next_function` | allowed |
| `goto_next_paragraph` | allowed |
| `goto_next_parameter` | allowed |
| `goto_next_tabstop` | allowed |
| `goto_next_test` | allowed |
| `goto_next_xml_element` | allowed |
| `goto_prev_change` | allowed |
| `goto_prev_class` | allowed |
| `goto_prev_comment` | allowed |
| `goto_prev_diag` | allowed |
| `goto_prev_entry` | allowed |
| `goto_prev_function` | allowed |
| `goto_prev_paragraph` | allowed |
| `goto_prev_parameter` | allowed |
| `goto_prev_tabstop` | allowed |
| `goto_prev_test` | allowed |
| `goto_prev_xml_element` | allowed |
| `goto_previous_buffer` | allowed |
| `goto_reference` | allowed |
| `goto_type_definition` | allowed |
| `goto_window_bottom` | allowed |
| `goto_window_center` | allowed |
| `goto_window_top` | allowed |
| `goto_word` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `half_page_down` | allowed |
| `half_page_up` | allowed |
| `hover` | allowed |
| `hsplit` | allowed |
| `hsplit_new` | allowed |
| `increment` | allowed |
| `indent` | allowed |
| `insert_at_line_end` | allowed |
| `insert_at_line_start` | allowed |
| `insert_char_interactive` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `insert_mode` | allowed |
| `insert_newline` | allowed |
| `insert_register` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `insert_tab` | allowed |
| `join_selections` | allowed |
| `join_selections_space` | allowed |
| `jump_backward` | allowed |
| `jump_forward` | allowed |
| `jump_view_down` | allowed |
| `jump_view_left` | allowed |
| `jump_view_right` | allowed |
| `jump_view_up` | allowed |
| `jumplist_picker` | allowed |
| `keep_primary_selection` | allowed |
| `keep_selections` | allowed |
| `kill_to_line_end` | allowed |
| `kill_to_line_start` | allowed |
| `last_picker` | allowed |
| `later` | allowed |
| `lsp_or_syntax_symbol_picker` | allowed |
| `lsp_or_syntax_workspace_symbol_picker` | allowed |
| `make_search_word_bounded` | allowed |
| `match_brackets` | allowed |
| `merge_consecutive_selections` | allowed |
| `merge_selections` | allowed |
| `move_char_left` | allowed |
| `move_char_right` | allowed |
| `move_line_down` | allowed |
| `move_line_up` | allowed |
| `move_next_long_word_end` | allowed |
| `move_next_long_word_start` | allowed |
| `move_next_sub_word_end` | allowed |
| `move_next_sub_word_start` | allowed |
| `move_next_word_end` | allowed |
| `move_next_word_start` | allowed |
| `move_parent_node_end` | allowed |
| `move_parent_node_start` | allowed |
| `move_prev_long_word_end` | allowed |
| `move_prev_long_word_start` | allowed |
| `move_prev_sub_word_end` | allowed |
| `move_prev_sub_word_start` | allowed |
| `move_prev_word_end` | allowed |
| `move_prev_word_start` | allowed |
| `move_visual_line_down` | allowed |
| `move_visual_line_up` | allowed |
| `no_op` | allowed |
| `normal_mode` | allowed |
| `open_above` | allowed |
| `open_below` | allowed |
| `page_cursor_down` | allowed |
| `page_cursor_half_down` | allowed |
| `page_cursor_half_up` | allowed |
| `page_cursor_up` | allowed |
| `page_down` | allowed |
| `page_up` | allowed |
| `paste_after` | allowed |
| `paste_before` | allowed |
| `paste_clipboard_after` | allowed |
| `paste_clipboard_before` | allowed |
| `paste_primary_clipboard_after` | allowed |
| `paste_primary_clipboard_before` | allowed |
| `record_macro` | allowed |
| `redo` | allowed |
| `remove_primary_selection` | allowed |
| `remove_selections` | allowed |
| `rename_symbol` | allowed |
| `repeat_last_motion` | allowed |
| `replace` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `replace_selections_with_clipboard` | allowed |
| `replace_selections_with_primary_clipboard` | allowed |
| `replace_with_yanked` | allowed |
| `replay_macro` | refused: replays keys from a register without checking them |
| `reverse_selection_contents` | allowed |
| `rotate_selection_contents_backward` | allowed |
| `rotate_selection_contents_forward` | allowed |
| `rotate_selections_backward` | allowed |
| `rotate_selections_first` | allowed |
| `rotate_selections_forward` | allowed |
| `rotate_selections_last` | allowed |
| `rotate_view` | allowed |
| `rotate_view_reverse` | allowed |
| `rsearch` | allowed |
| `save_selection` | allowed |
| `scroll_down` | allowed |
| `scroll_up` | allowed |
| `search` | allowed |
| `search_next` | allowed |
| `search_prev` | allowed |
| `search_selection` | allowed |
| `search_selection_detect_word_boundaries` | allowed |
| `select_all` | allowed |
| `select_all_children` | allowed |
| `select_all_siblings` | allowed |
| `select_line_above` | allowed |
| `select_line_below` | allowed |
| `select_mode` | allowed |
| `select_next_sibling` | allowed |
| `select_prev_sibling` | allowed |
| `select_references_to_symbol_under_cursor` | allowed |
| `select_regex` | allowed |
| `select_register` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `select_textobject_around` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `select_textobject_inner` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `shell_append_output` | refused: runs a shell command |
| `shell_insert_output` | refused: runs a shell command |
| `shell_keep_pipe` | refused: runs a shell command |
| `shell_pipe` | refused: runs a shell command |
| `shell_pipe_to` | refused: runs a shell command |
| `shrink_selection` | allowed |
| `shrink_to_line_bounds` | allowed |
| `signature_help` | allowed |
| `smart_tab` | allowed |
| `split_selection` | allowed |
| `split_selection_on_newline` | allowed |
| `surround_add` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `surround_delete` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `surround_replace` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `suspend` | refused: suspends Helix and hands the terminal to the shell |
| `swap_view_down` | allowed |
| `swap_view_left` | allowed |
| `swap_view_right` | allowed |
| `swap_view_up` | allowed |
| `switch_case` | allowed |
| `switch_to_lowercase` | allowed |
| `switch_to_uppercase` | allowed |
| `symbol_picker` | allowed |
| `syntax_symbol_picker` | allowed |
| `syntax_workspace_symbol_picker` | allowed |
| `till_prev_char` | allowed in `keys`; `static` answers `unsupported` (waits for the next key) |
| `toggle_block_comments` | allowed |
| `toggle_comments` | allowed |
| `toggle_line_comments` | allowed |
| `transpose_view` | allowed |
| `trim_selections` | allowed |
| `undo` | allowed |
| `unindent` | allowed |
| `vsplit` | allowed |
| `vsplit_new` | allowed |
| `wclose` | allowed |
| `wonly` | allowed |
| `workspace_diagnostics_picker` | allowed |
| `workspace_symbol_picker` | allowed |
| `yank` | allowed |
| `yank_joined` | allowed |
| `yank_joined_to_clipboard` | allowed |
| `yank_joined_to_primary_clipboard` | allowed |
| `yank_main_selection_to_clipboard` | allowed |
| `yank_main_selection_to_primary_clipboard` | allowed |
| `yank_to_clipboard` | allowed |
| `yank_to_primary_clipboard` | allowed |
