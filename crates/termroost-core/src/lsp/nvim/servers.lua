-- How Terminal Rooster's setup for Neovim runs each language server:
-- which files it serves, where its project starts, and its settings. The
-- command comes from installed.json (see init.lua).

local servers = {}

-- The top of a project: the nearest folder with .git or a lock file. In
-- one list, so the nearest wins, not a stray lock file further up (in the
-- home folder, say).
local project = { { '.git', 'package-lock.json', 'yarn.lock', 'pnpm-lock.yaml', 'bun.lock', 'bun.lockb' } }

-- The folder of the server `name` that runs already, for files reached
-- from a project but outside it (a crate's sources, decompiled code).
local function running_root(name)
  local clients = vim.lsp.get_clients({ name = name })
  return clients[#clients] and clients[#clients].config.root_dir
end

local function buf_file(bufnr)
  return vim.fs.normalize(vim.api.nvim_buf_get_name(bufnr))
end

local function is_under(dir, file)
  return vim.fs.relpath(vim.fs.normalize(dir), file) ~= nil
end

-- Rust ----------------------------------------------------------------

local workspaces = {}

servers['rust-analyzer'] = {
  filetypes = { 'rust' },
  root_dir = function(bufnr, on_dir)
    local file = buf_file(bufnr)
    local cargo = vim.env.CARGO_HOME or '~/.cargo'
    local rustup = vim.env.RUSTUP_HOME or '~/.rustup'
    for _, dir in ipairs({ cargo .. '/registry', cargo .. '/git', rustup .. '/toolchains' }) do
      if is_under(dir, file) then
        local root = running_root('rust-analyzer')
        if root then
          on_dir(root)
        end
        return
      end
    end
    local crate = vim.fs.root(bufnr, 'Cargo.toml')
    if not crate then
      return on_dir(vim.fs.root(bufnr, { 'rust-project.json', '.git' }))
    end
    if workspaces[crate] then
      return on_dir(workspaces[crate])
    end
    -- One server for a workspace, whichever of its crates a file is in.
    local cmd = { 'cargo', 'locate-project', '--workspace', '--message-format', 'plain' }
    local started = pcall(vim.system, cmd, { cwd = crate, text = true }, function(out)
      local manifest = out.code == 0 and vim.trim(out.stdout or '') or ''
      workspaces[crate] = manifest ~= '' and vim.fs.dirname(vim.fs.normalize(manifest)) or crate
      on_dir(workspaces[crate])
    end)
    if not started then
      on_dir(crate)
    end
  end,
  before_init = function(params, config)
    params.initializationOptions = config.settings and config.settings['rust-analyzer']
  end,
}

-- C# ------------------------------------------------------------------

-- Roslyn's own commands, which code actions and completions can carry.
local roslyn_commands = {}

local function apply(client, action)
  if action.edit then
    vim.lsp.util.apply_workspace_edit(action.edit, client.offset_encoding)
  end
  if action.command then
    client:exec_cmd(action.command)
  end
end

-- A completion that changes more than the word typed (overriding a method).
roslyn_commands['roslyn.client.completionComplexEdit'] = function(command, ctx)
  local client = assert(vim.lsp.get_client_by_id(ctx.client_id))
  local uri, edit = unpack(command.arguments or {})
  if uri and edit and edit.range then
    local changes = { [uri.uri] = { { range = edit.range, newText = edit.newText } } }
    vim.lsp.util.apply_workspace_edit({ changes = changes }, client.offset_encoding)
  end
end

-- A code action holding a choice of others ("Add using" for one of several
-- namespaces).
roslyn_commands['roslyn.client.nestedCodeAction'] = function(command, ctx)
  local client = assert(vim.lsp.get_client_by_id(ctx.client_id))
  local function run(action)
    if action.data and not action.edit and not action.command then
      return client:request('codeAction/resolve', action, function(err, resolved)
        if err then
          return vim.notify(err.message, vim.log.levels.ERROR)
        end
        run(resolved)
      end, ctx.bufnr)
    end
    local nested = action.NestedCodeActions
    if type(nested) ~= 'table' or #nested == 0 then
      return apply(client, action)
    end
    if #nested == 1 then
      return run(nested[1])
    end
    vim.ui.select(nested, {
      prompt = action.title,
      format_item = function(a)
        return a.title
      end,
    }, function(choice)
      if choice then
        run(choice)
      end
    end)
  end
  run(command.arguments[1])
end

-- "Fix all": in the document, the project or the solution.
roslyn_commands['roslyn.client.fixAllCodeAction'] = function(command, ctx)
  local client = assert(vim.lsp.get_client_by_id(ctx.client_id))
  local arg = command.arguments[1]
  vim.ui.select(arg.FixAllFlavors or {}, { prompt = 'Fix all in' }, function(scope)
    if not scope then
      return
    end
    local params = { title = command.title, data = arg, scope = scope }
    client:request('codeAction/resolveFixAll', params, function(err, resolved)
      if err then
        return vim.notify(err.message, vim.log.levels.ERROR)
      end
      apply(client, resolved)
    end, ctx.bufnr)
  end)
end

-- Roslyn gives a file's diagnostics when asked, one kind at a time
-- (compiler, analyzers...). Neovim asks as a file opens or changes, which
-- is before the project has loaded; this asks again for the open files
-- once it has, and whenever Roslyn says to.
local function pull_diagnostics(client_id)
  local client = vim.lsp.get_client_by_id(client_id)
  if not client then
    return
  end
  for bufnr in pairs(client.attached_buffers) do
    if vim.api.nvim_buf_is_loaded(bufnr) then
      -- The kinds for this file: Razor's, say, are for Razor files.
      for _, kind in ipairs(client.dynamic_capabilities:get('diagnosticProvider', { bufnr = bufnr }) or {}) do
        client:request('textDocument/diagnostic', {
          identifier = vim.tbl_get(kind, 'registerOptions', 'identifier'),
          textDocument = vim.lsp.util.make_text_document_params(bufnr),
        }, nil, bufnr)
      end
    end
  end
end

servers['roslyn'] = {
  filetypes = { 'cs' },
  -- The solution's folder, else the project's: Roslyn opens what it finds
  -- there itself (`--autoLoadProjects`).
  root_dir = function(bufnr, on_dir)
    local root = vim.fs.root(bufnr, function(name)
      return name:match('%.slnx?$') ~= nil
    end) or vim.fs.root(bufnr, function(name)
      return name:match('%.csproj$') ~= nil
    end) or running_root('roslyn')
    if root then
      on_dir(root)
    end
  end,
  commands = roslyn_commands,
  handlers = {
    ['workspace/projectInitializationComplete'] = function(_, _, ctx)
      pull_diagnostics(ctx.client_id)
      return vim.NIL
    end,
    -- Neovim would pull the workspace's diagnostics, which repeat the open
    -- files' (Roslyn checks only open files, so they hold nothing else).
    ['workspace/diagnostic/refresh'] = function(_, _, ctx)
      pull_diagnostics(ctx.client_id)
      return vim.NIL
    end,
    -- Razor needs Roslyn's Razor extension, which isn't set up.
    ['razor/provideDynamicFileInfo'] = function()
      return vim.NIL
    end,
  },
  settings = {
    ['csharp|completion'] = {
      dotnet_show_completion_items_from_unimported_namespaces = true,
      dotnet_show_name_completion_suggestions = true,
    },
  },
}

-- JavaScript and TypeScript ---------------------------------------------

local scripts = { 'javascript', 'javascriptreact', 'typescript', 'typescriptreact' }

servers['typescript'] = {
  filetypes = scripts,
  -- The whole project, also a monorepo: TypeScript finds each package's
  -- tsconfig.json itself.
  root_markers = project,
}

-- HTML, CSS and JSON ----------------------------------------------------

servers['html'] = {
  filetypes = { 'html' },
  root_markers = { { 'package.json', '.git' } },
  init_options = {
    provideFormatter = true,
    embeddedLanguages = { css = true, javascript = true },
    configurationSection = { 'html', 'css', 'javascript' },
  },
}

servers['css'] = {
  filetypes = { 'css', 'scss', 'less' },
  root_markers = { { 'package.json', '.git' } },
  init_options = { provideFormatter = true },
  settings = { css = { validate = true }, scss = { validate = true }, less = { validate = true } },
}

servers['json'] = {
  filetypes = { 'json', 'jsonc' },
  root_markers = { '.git' },
  init_options = { provideFormatter = true },
}

-- ESLint ----------------------------------------------------------------

local eslint_configs = {
  'eslint.config.js',
  'eslint.config.mjs',
  'eslint.config.cjs',
  'eslint.config.ts',
  'eslint.config.mts',
  'eslint.config.cts',
  '.eslintrc',
  '.eslintrc.js',
  '.eslintrc.cjs',
  '.eslintrc.json',
  '.eslintrc.yaml',
  '.eslintrc.yml',
}

servers['eslint'] = {
  filetypes = vim.list_extend({ 'vue', 'svelte', 'astro' }, scripts),
  -- Only in projects with an ESLint configuration.
  root_dir = function(bufnr, on_dir)
    local top = vim.fs.root(bufnr, project)
    local config = vim.fs.find(eslint_configs, {
      path = vim.fs.dirname(buf_file(bufnr)),
      upward = true,
      type = 'file',
      stop = top and vim.fs.dirname(top),
    })[1]
    if config then
      on_dir(top or vim.fs.dirname(config))
    end
  end,
  -- The server asks for all of these (they are VS Code's ESLint settings).
  settings = {
    validate = 'on',
    useESLintClass = false,
    experimental = {},
    codeActionOnSave = { enable = false, mode = 'all' },
    format = true,
    quiet = false,
    onIgnoredFiles = 'off',
    rulesCustomizations = {},
    run = 'onType',
    problems = { shortenToSingleLine = false },
    nodePath = '',
    workingDirectory = { mode = 'auto' },
    codeAction = {
      disableRuleComment = { enable = true, location = 'separateLine' },
      showDocumentation = { enable = true },
    },
  },
  before_init = function(_, config)
    if config.root_dir then
      config.settings.workspaceFolder = {
        uri = vim.uri_from_fname(config.root_dir),
        name = vim.fs.basename(config.root_dir),
      }
    end
  end,
  handlers = {
    ['eslint/openDoc'] = function(_, result)
      if result then
        vim.ui.open(result.url)
      end
      return {}
    end,
    -- Runs the project's own ESLint, as the project asks.
    ['eslint/confirmESLintExecution'] = function()
      return 4
    end,
    ['eslint/probeFailed'] = function()
      vim.notify('ESLint could not start in this project', vim.log.levels.WARN)
      return {}
    end,
    ['eslint/noLibrary'] = function()
      vim.notify("ESLint isn't installed in this project (npm install)", vim.log.levels.WARN)
      return {}
    end,
  },
}

-- Tailwind CSS ----------------------------------------------------------

servers['tailwind'] = {
  filetypes = vim.list_extend(
    { 'html', 'css', 'scss', 'less', 'postcss', 'vue', 'svelte', 'astro', 'razor', 'markdown', 'mdx' },
    scripts
  ),
  -- Only in projects that use Tailwind: with its configuration file, or
  -- (Tailwind 4, which needs none) with Tailwind in package.json.
  root_dir = function(bufnr, on_dir)
    local root = vim.fs.root(bufnr, function(name, dir)
      if name:match('^tailwind%.config%.[cm]?[jt]s$') then
        return true
      end
      if name ~= 'package.json' then
        return false
      end
      local file = io.open(vim.fs.joinpath(dir, name), 'r')
      local text = file and file:read('*a') or ''
      if file then
        file:close()
      end
      return text:find('tailwindcss', 1, true) ~= nil
    end)
    if root then
      on_dir(root)
    end
  end,
  capabilities = { workspace = { didChangeWatchedFiles = { dynamicRegistration = true } } },
  settings = {
    tailwindCSS = {
      validate = true,
      classAttributes = { 'class', 'className', 'class:list', 'classList', 'ngClass' },
    },
  },
  before_init = function(_, config)
    config.settings.editor = { tabSize = vim.lsp.util.get_effective_tabstop() }
  end,
}

return servers
