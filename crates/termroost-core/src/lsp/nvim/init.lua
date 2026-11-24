-- Terminal Rooster's setup for Neovim: the language servers Terminal
-- Rooster installed, with completion and diagnostics; Neovim's own
-- defaults otherwise. Terminal Rooster writes this folder, so changes made
-- here are lost. To run your own init.lua instead, turn the setup off in
-- Terminal Rooster's settings (Editor).

local here = vim.fs.dirname(vim.fs.normalize(debug.getinfo(1, 'S').source:sub(2)))

vim.o.number = true
vim.o.signcolumn = 'yes'
vim.o.ignorecase = true
vim.o.smartcase = true
vim.o.updatetime = 250
vim.o.completeopt = 'menuone,noselect,popup,fuzzy'
if vim.fn.has('clipboard') == 1 then
  vim.o.clipboard = 'unnamedplus'
end

vim.diagnostic.config({ virtual_text = true, severity_sort = true })

local servers = dofile(vim.fs.joinpath(here, 'servers.lua'))

-- What each server was last started with, to start it again only when
-- that changes.
local enabled = {}

-- Starts the servers installed.json lists. It says how to run each one
-- Terminal Rooster installed: `{ "<name>": { "cmd": [...], "settings": {...} } }`.
local function enable_installed()
  local file = io.open(vim.fs.joinpath(here, 'installed.json'), 'r')
  if not file then
    return
  end
  local ok, installed = pcall(vim.json.decode, file:read('*a'))
  file:close()
  if not ok or type(installed) ~= 'table' then
    return
  end
  for name, how in pairs(installed) do
    local key = vim.json.encode(how)
    if servers[name] and enabled[name] ~= key then
      enabled[name] = key
      local config = vim.tbl_deep_extend('force', servers[name], { cmd = how.cmd })
      if how.settings then
        config.settings = vim.tbl_deep_extend('force', config.settings or {}, how.settings)
      end
      vim.lsp.config(name, config)
      vim.lsp.enable(name)
    end
  end
end

enable_installed()

-- Languages Terminal Rooster installs while Neovim runs start right away,
-- also in the files already open.
local watcher = vim.uv.new_fs_event()
if watcher then
  watcher:start(here, {}, vim.schedule_wrap(function(err, name)
    if not err and name == 'installed.json' then
      enable_installed()
    end
  end))
end

vim.api.nvim_create_autocmd('LspAttach', {
  group = vim.api.nvim_create_augroup('terminalrooster.lsp', {}),
  callback = function(ev)
    local client = vim.lsp.get_client_by_id(ev.data.client_id)
    if client and client:supports_method('textDocument/completion') then
      vim.lsp.completion.enable(true, client.id, ev.buf, { autotrigger = true })
    end
    vim.keymap.set('n', 'gd', vim.lsp.buf.definition, { buffer = ev.buf, desc = 'vim.lsp.buf.definition()' })
  end,
})
