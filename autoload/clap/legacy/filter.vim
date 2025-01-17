" Author: liuchengxu <xuliuchengxlc@gmail.com>
" Description: Filter out the candidate lines synchorously given the input.
"
" NOTE: Deprecated as now the filtering is entiredly asynchrously done on the
" Rust end.

let s:save_cpo = &cpoptions
set cpoptions&vim

let s:can_use_lua = has('nvim-0.5') || has('lua') ? v:true : v:false

let s:MIDIUM_CAPACITY = 30000

if exists('g:clap_builtin_fuzzy_filter_threshold')
  let s:builtin_filter_capacity = g:clap_builtin_fuzzy_filter_threshold
elseif s:can_use_lua
  let s:builtin_filter_capacity = s:MIDIUM_CAPACITY
else
  let s:builtin_filter_capacity = 10000
endif

let s:related_builtin_providers = ['tags', 'buffers', 'files', 'git_files', 'history', 'filer', 'grep', 'live_grep']

function! s:enable_icon() abort
  if g:clap_enable_icon
        \ && index(s:related_builtin_providers, g:clap.provider.id) > -1
    return v:true
  else
    return v:false
  endif
endfunction

function! clap#legacy#filter#get_bonus_type() abort
  if index(['files', 'git_files', 'filer'], g:clap.provider.id) > -1
    return 'FileName'
  else
    return 'None'
  endif
endfunction

function! clap#legacy#filter#matchfuzzy(query, candidates) abort
  " `result` could be a list of two lists, or a list of three
  " lists(newer vim).
  let result = matchfuzzypos(a:candidates, a:query)
  let filtered = result[0]
  let matched_indices = result[1]
  if s:enable_icon()
    let g:__clap_fuzzy_matched_indices = []
    for indices in matched_indices
      call add(g:__clap_fuzzy_matched_indices, map(indices, 'v:val + 2'))
    endfor
  else
    let g:__clap_fuzzy_matched_indices = matched_indices
  endif
  return filtered
endfunction

function! s:match_scope() abort
  return exists('g:__clap_match_scope_enum') ? g:__clap_match_scope_enum : 'Full'
endfunction

if get(g:, 'clap_force_matchfuzzy', v:false)
  let s:current_filter_impl = 'VimL'
  if !exists('*matchfuzzypos')
    call clap#helper#echo_error('matchfuzzypos not found, please upgrade your Vim')
    finish
  endif
  let s:builtin_filter_capacity = s:MIDIUM_CAPACITY
  function! clap#legacy#filter#sync(query, candidates) abort
    return clap#legacy#filter#matchfuzzy(a:query, a:candidates)
  endfunction
elseif s:can_use_lua && !get(g:, 'clap_force_python', v:false)
  let s:current_filter_impl = 'Lua'
  function! clap#legacy#filter#sync(query, candidates) abort
    return clap#legacy#filter#sync#lua#(a:query, a:candidates, -1, s:enable_icon(), s:match_scope())
  endfunction
else
  let s:can_use_python = v:false
  let s:has_py_dynamic_module = v:false

  if has('python3') || has('python')
    try
      let s:has_py_dynamic_module = clap#legacy#filter#sync#python#has_dynamic_module()
      let s:can_use_python = v:true
    catch
      call clap#helper#echo_error(v:exception)
    endtry
  endif

  if s:has_py_dynamic_module
    let s:builtin_filter_capacity = s:MIDIUM_CAPACITY
  endif

  if s:can_use_python
    let s:current_filter_impl = 'Python'

    function! clap#legacy#filter#sync(query, candidates) abort
      " All the values of context will be treated as PyString in PyO3.
      let context = {
            \ 'winwidth': winwidth(g:clap.display.winid),
            \ 'enable_icon': s:enable_icon() == v:true ? 'True' : 'False',
            \ 'match_scope': s:match_scope(),
            \ 'bonus_type': clap#legacy#filter#get_bonus_type(),
            \ }
      " TODO: support more providers by detecting if the specific
      " file exists in the project root? Cargo.toml(rs), go.mod(go), ...
      if g:clap.provider.id ==# 'blines'
        let context['language'] = expand('#'.g:clap.start.bufnr.':e')
      endif
      try
        return clap#legacy#filter#sync#python#(a:query, a:candidates, clap#util#recent_files(), context)
      catch
        call clap#helper#echo_error(v:exception.', throwpoint:'.v:throwpoint)
        return clap#legacy#filter#sync#viml#(a:query, a:candidates)
      endtry
    endfunction
  else
    let s:current_filter_impl = 'VimL'
    if exists('*matchfuzzypos')
      let s:builtin_filter_capacity = s:MIDIUM_CAPACITY
      function! clap#legacy#filter#sync(query, candidates) abort
        return clap#legacy#filter#matchfuzzy(a:query, a:candidates)
      endfunction
    else
      function! clap#legacy#filter#sync(query, candidates) abort
        return clap#legacy#filter#sync#viml#(a:query, a:candidates)
      endfunction
    endif
  endif

endif

function! clap#legacy#filter#on_typed(FilterFn, query, candidates) abort
  let l:lines = a:FilterFn(a:query, a:candidates)

  if empty(l:lines)
    let l:lines = [g:clap_no_matches_msg]
    let g:__clap_has_no_matches = v:true
    call g:clap.display.set_lines_lazy(lines)
    " In clap#state#refresh_matches_count() we reset the sign to the first line,
    " But the signs are seemingly removed when setting the lines, so we should
    " postpone the sign update.
    call clap#state#refresh_matches_count(0)
    if exists('g:__clap_lines_truncated_map')
      unlet g:__clap_lines_truncated_map
    endif
    if clap#preview#is_enabled()
      call g:clap.preview.clear()
      call g:clap.preview.hide()
    endif
  else
    let g:__clap_has_no_matches = v:false
    call g:clap.display.set_lines_lazy(lines)
    call clap#state#refresh_matches_count(len(l:lines))
  endif

  call g:clap#display_win.shrink_if_undersize()
  call clap#spinner#set_idle()

  if !g:__clap_has_no_matches
    if exists('g:__clap_fuzzy_matched_indices')
      " Due the cache strategy, g:__clap_fuzzy_matched_indices may be oversize
      " than the actual display buffer, the rest highlight indices of g:__clap_fuzzy_matched_indices
      " belong to the cached lines.
      "
      " TODO: also add highlights for the cached lines?
      let hl_lines = g:__clap_fuzzy_matched_indices[:g:clap.display.line_count()-1]
      call clap#highlighter#add_highlights(hl_lines)
    else
      call g:clap.display.legacy_add_highlight()
    endif
  endif
endfunction

function! clap#legacy#filter#beyond_capacity(size) abort
  return a:size > s:builtin_filter_capacity
endfunction

function! clap#legacy#filter#capacity() abort
  return s:builtin_filter_capacity
endfunction

function! clap#legacy#filter#current_impl() abort
  return s:current_filter_impl
endfunction

let &cpoptions = s:save_cpo
unlet s:save_cpo
