'use strict';

const vscode = require('vscode');
const childProcess = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const COMMAND_IDS = Object.freeze({
  build: 'hardscript.build',
  run: 'hardscript.run',
  test: 'hardscript.test',
  format: 'hardscript.format',
  doctor: 'hardscript.doctor',
  explainError: 'hardscript.explainError'
});

const MAX_CAPTURE = 2 * 1024 * 1024;
let extensionContext;
let outputChannel;
let languageClient;
let clientStartPromise;
let languageClientApi;
const activeProcesses = new Set();

function loadLanguageClientApi() {
  if (!languageClientApi) {
    const api = require('vscode-languageclient/node');
    languageClientApi = {
      LanguageClient: api.LanguageClient,
      TransportKind: api.TransportKind
    };
  }
  return languageClientApi;
}

function log(message) {
  if (!outputChannel) {
    return;
  }
  const text = message instanceof Error ? message.stack || message.message : String(message);
  if (text && typeof outputChannel.appendLine === 'function') {
    try {
      outputChannel.appendLine(text);
    } catch (_) {
      return;
    }
  }
}

function notifyError(message) {
  const text = message instanceof Error ? message.message : String(message);
  log(text);
  if (vscode.window && typeof vscode.window.showErrorMessage === 'function') {
    Promise.resolve(vscode.window.showErrorMessage(text)).catch(() => undefined);
  }
}

function notifyInformation(message) {
  const text = message instanceof Error ? message.message : String(message);
  log(text);
  if (vscode.window && typeof vscode.window.showInformationMessage === 'function') {
    Promise.resolve(vscode.window.showInformationMessage(text)).catch(() => undefined);
  }
}

function workspaceFolders() {
  if (!vscode.workspace || !Array.isArray(vscode.workspace.workspaceFolders)) {
    return [];
  }
  return vscode.workspace.workspaceFolders.slice();
}

function folderPath(folder) {
  if (!folder) {
    return '';
  }
  if (typeof folder === 'string') {
    return folder;
  }
  if (folder.uri && typeof folder.uri.fsPath === 'string' && folder.uri.fsPath) {
    return folder.uri.fsPath;
  }
  if (folder.uri && typeof folder.uri.path === 'string' && folder.uri.path) {
    return folder.uri.path;
  }
  return '';
}

function folderUri(folder) {
  if (!folder) {
    return '';
  }
  if (folder.uri && typeof folder.uri.toString === 'function') {
    return folder.uri.toString();
  }
  if (folder.uri && typeof folder.uri.path === 'string') {
    return folder.uri.path;
  }
  return folderPath(folder);
}

function workspaceFolderForUri(uri) {
  if (!uri || !vscode.workspace || typeof vscode.workspace.getWorkspaceFolder !== 'function') {
    return undefined;
  }
  try {
    return vscode.workspace.getWorkspaceFolder(uri);
  } catch (_) {
    return undefined;
  }
}

function activeWorkspaceFolder() {
  const editor = vscode.window && vscode.window.activeTextEditor;
  if (editor && editor.document) {
    const folder = workspaceFolderForUri(editor.document.uri);
    if (folder) {
      return folder;
    }
  }
  return workspaceFolders()[0];
}

function foldersFor(folder) {
  const result = [];
  const add = value => {
    if (!value) {
      return;
    }
    const valuePath = folderPath(value);
    if (valuePath && !result.some(item => folderPath(item) === valuePath)) {
      result.push(value);
    }
  };
  add(folder);
  workspaceFolders().forEach(add);
  return result;
}

function normalizeSetting(value) {
  if (typeof value === 'string') {
    const result = value.trim();
    return result || null;
  }
  if (Array.isArray(value)) {
    return value.map(item => String(item));
  }
  return null;
}

function setting(folder, name) {
  let value;
  try {
    const resource = folder && folder.uri ? folder.uri : undefined;
    const config = vscode.workspace.getConfiguration('hardscript', resource);
    value = config && typeof config.get === 'function' ? config.get(name) : undefined;
  } catch (_) {
    value = undefined;
  }
  if (value === undefined || value === null || value === '') {
    try {
      const config = vscode.workspace.getConfiguration('hardscript');
      value = config && typeof config.get === 'function' ? config.get(name) : undefined;
    } catch (_) {
      value = undefined;
    }
  }
  return value;
}

function explicitPath(folder, kind) {
  const names = kind === 'server' ? ['serverPath'] : ['cliPath', 'hardPath'];
  for (const name of names) {
    const value = normalizeSetting(setting(folder, name));
    if (typeof value === 'string') {
      return value;
    }
  }
  return null;
}

function platformNames(names) {
  if (process.platform !== 'win32') {
    return names.slice();
  }
  const result = [];
  const extensions = (process.env.PATHEXT || '.EXE').split(';').filter(Boolean);
  for (const name of names) {
    result.push(name);
    if (!path.extname(name)) {
      for (const extension of extensions) {
        result.push(`${name}${extension}`);
      }
    }
  }
  return result;
}

function regularFile(file) {
  try {
    if (typeof fs.statSync !== 'function') {
      return Boolean(fs.existsSync(file));
    }
    return fs.statSync(file).isFile();
  } catch (_) {
    return false;
  }
}

function expandPath(value, folder) {
  let result = value;
  const root = folderPath(folder);
  if (result === '~') {
    return os.homedir();
  }
  if (result.startsWith(`~${path.sep}`) || result.startsWith('~/')) {
    result = path.join(os.homedir(), result.slice(2));
  }
  if (root) {
    result = result.replace(/\$\{workspaceFolder\}/g, root);
    result = result.replace(/\$\{workspaceRoot\}/g, root);
  }
  return result;
}

function hasPathSeparator(value) {
  return value.includes('/') || value.includes('\\');
}

function resolveExplicit(value, folders) {
  if (!value) {
    return null;
  }
  const expanded = expandPath(value, folders[0]);
  if (hasPathSeparator(expanded) || path.isAbsolute(expanded)) {
    const bases = folders.map(folderPath).filter(Boolean);
    if (!bases.length) {
      bases.push(process.cwd());
    }
    for (const base of bases) {
      const candidate = path.resolve(base, expanded);
      if (regularFile(candidate)) {
        return candidate;
      }
    }
    return null;
  }
  return findOnPath([expanded]);
}

function pathEntries() {
  const environmentPath = process.env.PATH || process.env.Path || '';
  if (!environmentPath) {
    return [];
  }
  return environmentPath.split(path.delimiter).filter(Boolean);
}

function findOnPath(names) {
  const expandedNames = platformNames(names);
  for (const entry of pathEntries()) {
    const base = path.isAbsolute(entry) ? entry : path.resolve(process.cwd(), entry);
    for (const name of expandedNames) {
      const candidate = path.resolve(base, name);
      if (regularFile(candidate)) {
        return candidate;
      }
    }
  }
  return null;
}

function workspaceBinaryCandidates(names, folders) {
  const candidates = [];
  const expandedNames = platformNames(names);
  for (const folder of folders) {
    const root = folderPath(folder);
    if (!root) {
      continue;
    }
    for (const profile of ['release', 'debug']) {
      for (const name of expandedNames) {
        candidates.push(path.join(root, 'target', profile, name));
      }
    }
  }
  return candidates;
}

function bundledBinaryCandidates(names, extensionPath) {
  if (!extensionPath) {
    return [];
  }
  const candidates = [];
  const expandedNames = platformNames(names);
  for (const name of expandedNames) {
    candidates.push(path.join(extensionPath, 'bin', name));
    candidates.push(path.join(extensionPath, 'server', name));
    candidates.push(path.join(extensionPath, 'resources', 'bin', name));
    candidates.push(path.join(extensionPath, name));
  }
  return candidates;
}

function resolveBinary(kind, options) {
  const opts = options || {};
  const names = kind === 'server'
    ? ['hs-lsp', 'hardscript-lsp', 'hard-lsp']
    : ['hard'];
  const folders = foldersFor(opts.folder || opts.workspaceFolder);
  const explicit = opts.explicit === undefined ? explicitPath(opts.folder || opts.workspaceFolder, kind) : opts.explicit;
  const fromSetting = resolveExplicit(explicit, folders);
  if (fromSetting) {
    return { command: fromSetting, source: 'setting' };
  }

  for (const candidate of workspaceBinaryCandidates(names, folders)) {
    if (regularFile(candidate)) {
      return { command: candidate, source: 'workspace' };
    }
  }

  const extensionPath = opts.extensionPath || (extensionContext && extensionContext.extensionPath);
  for (const candidate of bundledBinaryCandidates(names, extensionPath)) {
    if (regularFile(candidate)) {
      return { command: candidate, source: 'bundled' };
    }
  }

  const fromPath = findOnPath(names);
  if (fromPath) {
    return { command: fromPath, source: 'path' };
  }
  return null;
}

function resolveServerExecutable(folder, explicit) {
  return resolveBinary('server', {
    folder,
    explicit,
    extensionPath: extensionContext && extensionContext.extensionPath
  });
}

function resolveCliExecutable(folder, explicit) {
  return resolveBinary('cli', {
    folder,
    explicit,
    extensionPath: extensionContext && extensionContext.extensionPath
  });
}

function resolveServerPath(folder, explicit) {
  const resolved = resolveServerExecutable(folder, explicit);
  return resolved ? resolved.command : null;
}

function resolveHardPath(folder, explicit) {
  const resolved = resolveCliExecutable(folder, explicit);
  return resolved ? resolved.command : null;
}

function serverArgs(folder) {
  const value = normalizeSetting(setting(folder, 'serverArgs'));
  return Array.isArray(value) ? value.map(item => expandPath(String(item), folder)) : [];
}

function workspaceInitializationOptions(primaryFolder) {
  const primary = primaryFolder || activeWorkspaceFolder();
  const folders = foldersFor(primary);
  return {
    workspaceFolders: folders.map(folder => ({
      uri: folderUri(folder),
      name: folder.name || path.basename(folderPath(folder))
    })),
    primaryWorkspaceFolder: folderUri(primary),
    rootUri: folderUri(primary),
    rootPath: folderPath(primary)
  };
}

function createStdioServerOptions(server, folder) {
  const api = loadLanguageClientApi();
  const transport = api.TransportKind && api.TransportKind.stdio
    ? api.TransportKind.stdio
    : 'stdio';
  const cwd = folderPath(folder) || process.cwd();
  const executable = {
    command: server.command,
    args: server.args || [],
    transport,
    options: {
      cwd,
      env: process.env,
      windowsHide: true
    }
  };
  return {
    run: executable,
    debug: executable
  };
}

async function startLanguageClient() {
  if (clientStartPromise) {
    return clientStartPromise;
  }
  const folder = activeWorkspaceFolder();
  const server = resolveServerExecutable(folder);
  if (!server) {
    log('HardScript language server was not found. Set hardscript.serverPath or build hs-lsp in target/release or target/debug.');
    return null;
  }
  let api;
  try {
    api = loadLanguageClientApi();
  } catch (error) {
    log(`HardScript language client dependency could not be loaded: ${error.message}`);
    return null;
  }
  let nextClient;
  try {
    const serverOptions = createStdioServerOptions({
      command: server.command,
      args: serverArgs(folder)
    }, folder);
    const clientOptions = {
      documentSelector: [
        { scheme: 'file', language: 'hardscript' },
        { scheme: 'untitled', language: 'hardscript' }
      ],
      initializationOptions: workspaceInitializationOptions(folder),
      outputChannel,
      revealOutputChannelOn: 4
    };
    const trace = setting(folder, 'trace.server');
    if (trace === 'messages') {
      clientOptions.trace = 1;
    } else if (trace === 'verbose') {
      clientOptions.trace = 3;
    }
    const watcher = vscode.workspace && typeof vscode.workspace.createFileSystemWatcher === 'function'
      ? vscode.workspace.createFileSystemWatcher('**/*.hard')
      : null;
    if (watcher) {
      clientOptions.synchronize = { fileEvents: watcher };
      if (extensionContext) {
        extensionContext.subscriptions.push(watcher);
      }
    }
    if (folder) {
      clientOptions.workspaceFolder = folder;
    }
    nextClient = new api.LanguageClient(
      'hardscript',
      'HardScript Language Server',
      serverOptions,
      clientOptions
    );
    languageClient = nextClient;
  } catch (error) {
    languageClient = null;
    log(`HardScript language client could not be created: ${error.message}`);
    return null;
  }
  clientStartPromise = Promise.resolve()
    .then(() => nextClient.start())
    .then(() => nextClient)
    .catch(error => {
      languageClient = null;
      log(`HardScript language server exited while starting: ${error.message}`);
      return null;
    });
  return clientStartPromise;
}

function startLanguageClientSafely() {
  return Promise.resolve()
    .then(() => startLanguageClient())
    .catch(error => {
      log(`HardScript language client failed: ${error.message}`);
      return null;
    });
}

async function stopLanguageClient() {
  const current = languageClient;
  languageClient = null;
  clientStartPromise = null;
  if (!current || typeof current.stop !== 'function') {
    return;
  }
  try {
    await current.stop();
  } catch (error) {
    log(`HardScript language server did not stop cleanly: ${error.message}`);
  }
}

function outputText(value) {
  if (value === undefined || value === null) {
    return '';
  }
  return String(value);
}

function appendProcessOutput(label, stream, state) {
  if (!stream || typeof stream.on !== 'function') {
    return;
  }
  if (typeof stream.setEncoding === 'function') {
    stream.setEncoding('utf8');
  }
  stream.on('data', chunk => {
    const text = outputText(chunk);
    if (state.text.length < MAX_CAPTURE) {
      state.text += text.slice(0, MAX_CAPTURE - state.text.length);
    }
    if (outputChannel && typeof outputChannel.append === 'function') {
      try {
        outputChannel.append(`[HardScript ${label}] ${text}`);
      } catch (_) {
        return;
      }
    }
  });
}

function spawnProcess(command, args, options) {
  const opts = options || {};
  return new Promise((resolve, reject) => {
    const cwd = opts.cwd || process.cwd();
    const stdout = { text: '' };
    const stderr = { text: '' };
    let child;
    let settled = false;
    const finish = (callback, value) => {
      if (settled) {
        return;
      }
      settled = true;
      if (child) {
        activeProcesses.delete(child);
      }
      callback(value);
    };
    try {
      child = childProcess.spawn(command, args, {
        cwd,
        env: opts.env || process.env,
        stdio: ['ignore', 'pipe', 'pipe'],
        windowsHide: true
      });
    } catch (error) {
      finish(reject, error);
      return;
    }
    activeProcesses.add(child);
    appendProcessOutput(opts.label || 'hard', child.stdout, stdout);
    appendProcessOutput(opts.label || 'hard', child.stderr, stderr);
    child.once('error', error => {
      finish(reject, error);
    });
    child.once('close', (code, signal) => {
      finish(resolve, {
        code: typeof code === 'number' ? code : 1,
        signal: signal || null,
        stdout: stdout.text,
        stderr: stderr.text
      });
    });
  });
}

function spawnHard(args, options) {
  const opts = options || {};
  const folder = opts.folder || activeWorkspaceFolder();
  const resolved = resolveCliExecutable(folder);
  if (!resolved) {
    return Promise.reject(new Error('Could not find hard. Set hardscript.cliPath, use target/release/hard or target/debug/hard, bundle hard, or add it to PATH.'));
  }
  const cwd = folderPath(folder) || process.cwd();
  log(`Running ${resolved.command} ${args.join(' ')}`);
  return spawnProcess(resolved.command, args, {
    cwd,
    env: process.env,
    label: opts.label || 'hard'
  });
}

function lastOutput(result) {
  const text = [result.stderr, result.stdout]
    .filter(Boolean)
    .join('\n')
    .trim();
  if (!text) {
    return '';
  }
  const lines = text.split(/\r?\n/);
  return lines.slice(-8).join('\n');
}

function resultError(label, result) {
  const exit = result.signal ? `signal ${result.signal}` : `exit code ${result.code}`;
  const detail = lastOutput(result);
  return detail ? `HardScript ${label} failed (${exit}).\n${detail}` : `HardScript ${label} failed (${exit}).`;
}

function isHardDocument(document) {
  if (!document) {
    return false;
  }
  if (document.languageId === 'hardscript') {
    return true;
  }
  const file = document.uri && (document.uri.fsPath || document.uri.path);
  return typeof file === 'string' && file.toLowerCase().endsWith('.hard');
}

async function runCliCommand(label, subcommand, includeTarget, extraArgs) {
  const editor = vscode.window && vscode.window.activeTextEditor;
  const document = editor && editor.document;
  const folder = document ? (workspaceFolderForUri(document.uri) || activeWorkspaceFolder()) : activeWorkspaceFolder();
  const sourceDocument = isHardDocument(document) ? document : null;
  const args = [subcommand];
  if (includeTarget) {
    args.push(targetArgument(sourceDocument, folder));
  }
  if (Array.isArray(extraArgs)) {
    args.push(...extraArgs.map(String));
  }
  void startLanguageClientSafely();
  try {
    const result = await spawnHard(args, { folder, label: subcommand });
    if (result.code !== 0 || result.signal) {
      notifyError(resultError(subcommand, result));
    } else if (label !== 'run') {
      notifyInformation(`HardScript ${label} completed.`);
    }
    return result;
  } catch (error) {
    notifyError(error);
    return null;
  }
}

function targetArgument(document, folder) {
  if (!document || !document.uri) {
    return 'main.hard';
  }
  const absolute = document.uri.fsPath || document.uri.path;
  if (!absolute) {
    return 'main.hard';
  }
  const root = folderPath(folder);
  if (root) {
    const relative = path.relative(root, absolute);
    if (relative && !relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative)) {
      return relative;
    }
  }
  return absolute;
}

function diagnosticCode(diagnostic) {
  if (!diagnostic) {
    return null;
  }
  const values = [];
  if (diagnostic.code !== undefined && diagnostic.code !== null) {
    values.push(diagnostic.code);
  }
  if (diagnostic.message) {
    values.push(diagnostic.message);
  }
  for (const value of values) {
    const text = typeof value === 'object' && value !== null && value.value !== undefined
      ? value.value
      : value;
    if (typeof text === 'number' && Number.isInteger(text) && text >= 0) {
      return `HS${String(text).padStart(4, '0')}`;
    }
    const match = outputText(text).match(/\bHS\d{4}\b/i);
    if (match) {
      return match[0].toUpperCase();
    }
  }
  return null;
}

function rangesOverlap(left, right) {
  if (!left || !right || !left.start || !right.start || !left.end || !right.end) {
    return false;
  }
  const before = left.end.line < right.start.line
    || (left.end.line === right.start.line && left.end.character <= right.start.character);
  const after = right.end.line < left.start.line
    || (right.end.line === left.start.line && right.end.character <= left.start.character);
  return !before && !after;
}

function selectedCode(editor) {
  if (!editor || !editor.document || typeof editor.document.getText !== 'function') {
    return null;
  }
  const selection = editor.selection;
  if (!selection) {
    return null;
  }
  try {
    const text = editor.document.getText(selection);
    const match = outputText(text).match(/\bHS\d{4}\b/i);
    return match ? match[0].toUpperCase() : null;
  } catch (_) {
    return null;
  }
}

function diagnosticsFor(editor) {
  if (!editor || !editor.document || !vscode.languages || typeof vscode.languages.getDiagnostics !== 'function') {
    return [];
  }
  try {
    const diagnostics = vscode.languages.getDiagnostics(editor.document.uri);
    return Array.isArray(diagnostics) ? diagnostics : [];
  } catch (_) {
    return [];
  }
}

function findDiagnosticCode(editor) {
  const selected = selectedCode(editor);
  if (selected) {
    return selected;
  }
  const diagnostics = diagnosticsFor(editor);
  if (!diagnostics.length) {
    return null;
  }
  const selection = editor.selection;
  const ordered = diagnostics.slice().sort((left, right) => {
    const leftTouches = selection && left.range ? Number(rangesOverlap(selection, left.range)) : 0;
    const rightTouches = selection && right.range ? Number(rangesOverlap(selection, right.range)) : 0;
    return rightTouches - leftTouches;
  });
  for (const diagnostic of ordered) {
    const code = diagnosticCode(diagnostic);
    if (code) {
      return code;
    }
  }
  return null;
}

function withTimeout(promise, milliseconds) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error('request timed out')), milliseconds);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

function serverExplanationText(value) {
  if (typeof value === 'string') {
    return value;
  }
  if (!value || typeof value !== 'object') {
    return '';
  }
  for (const key of ['explanation', 'message', 'text', 'content', 'description']) {
    if (typeof value[key] === 'string' && value[key].trim()) {
      return value[key];
    }
  }
  return '';
}

async function requestServerExplanation(code) {
  const client = await startLanguageClientSafely();
  if (!client || typeof client.sendRequest !== 'function') {
    return '';
  }
  try {
    const result = await withTimeout(
      client.sendRequest('workspace/executeCommand', {
        command: 'hardscript.explainError',
        arguments: [code]
      }),
      2000
    );
    return serverExplanationText(result);
  } catch (error) {
    log(`Language server could not explain ${code}: ${error.message}`);
    return '';
  }
}

function catalogEntry(output, code) {
  const text = outputText(output);
  if (!text) {
    return '';
  }
  const escaped = code.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const markdown = new RegExp(`###\\s+\`${escaped}\`([\\s\\S]*?)(?=\\n###\\s+\`HS\\d{4}\`|\\n##\\s|$)`, 'i').exec(text);
  if (markdown) {
    return markdown[0].trim();
  }
  const line = new RegExp(`^.*\\b${escaped}\\b.*$`, 'im').exec(text);
  return line ? line[0].trim() : '';
}

function firstLine(value) {
  const text = outputText(value).trim();
  if (!text) {
    return '';
  }
  const lines = text.split(/\r?\n/).map(line => line.trim()).filter(Boolean);
  return lines[0] || '';
}

function normalizedCode(value) {
  const text = outputText(value).trim();
  const match = /\bHS\d{4}\b/i.exec(text);
  return match ? match[0].toUpperCase() : null;
}

async function explainError(requestedCode) {
  const editor = vscode.window && vscode.window.activeTextEditor;
  const code = normalizedCode(requestedCode) || findDiagnosticCode(editor);
  if (!code) {
    notifyInformation('HardScript: place the cursor on a diagnostic or select an HSxxxx code to explain it.');
    return null;
  }
  const folder = editor && editor.document ? (workspaceFolderForUri(editor.document.uri) || activeWorkspaceFolder()) : activeWorkspaceFolder();
  void startLanguageClientSafely();
  const serverText = await requestServerExplanation(code);
  let catalogText = '';
  let catalogError = null;
  try {
    const result = await spawnHard(['errors', '--markdown'], { folder, label: 'errors' });
    if (result.code === 0 && !result.signal) {
      catalogText = catalogEntry(result.stdout, code);
    } else {
      catalogError = resultError('errors', result);
    }
  } catch (error) {
    catalogError = error.message;
  }
  const diagnostic = diagnosticsFor(editor).find(item => diagnosticCode(item) === code) || null;
  const fallback = diagnostic && diagnostic.message ? diagnostic.message : '';
  const explanation = serverText || catalogText || fallback;
  if (explanation) {
    log(`${code}\n${explanation}`);
    const summary = firstLine(explanation);
    notifyInformation(summary ? `${code}: ${summary}` : `HardScript explanation for ${code} is available in the HardScript output channel.`);
  } else {
    const detail = catalogError ? ` ${catalogError}` : '';
    notifyInformation(`HardScript could not find an explanation for ${code}.${detail}`);
  }
  return explanation;
}

const TASK_COMMANDS = Object.freeze(['build', 'run', 'test', 'format', 'doctor', 'debug']);
const TASK_PROBLEM_MATCHER = Object.freeze(['$hardscript']);

function taskFolder(task) {
  if (task && task.scope && task.scope.workspaceFolder) {
    return task.scope.workspaceFolder;
  }
  return activeWorkspaceFolder();
}

function activeHardFile(folder) {
  const editor = vscode.window && vscode.window.activeTextEditor;
  if (editor && isHardDocument(editor.document)) {
    const absolute = editor.document.uri.fsPath || editor.document.uri.path;
    const root = folderPath(folder) || process.cwd();
    const relative = path.relative(root, absolute);
    if (relative && !relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative)) {
      return relative.split(path.sep).join('/');
    }
    return absolute;
  }
  return '';
}

function hardFilesIn(directory) {
  try {
    return fs.readdirSync(directory, { withFileTypes: true })
      .filter(entry => entry.isFile() && entry.name.endsWith('.hard'))
      .map(entry => entry.name)
      .sort();
  } catch (_) {
    return [];
  }
}

function subdirectoriesOf(directory) {
  try {
    return fs.readdirSync(directory, { withFileTypes: true })
      .filter(entry => entry.isDirectory())
      .map(entry => entry.name)
      .sort();
  } catch (_) {
    return [];
  }
}

function directoryExists(directory) {
  try {
    return fs.statSync(directory).isDirectory();
  } catch (_) {
    return false;
  }
}

function preferredHardFile(directory) {
  const files = hardFilesIn(directory);
  if (files.includes('main.hard')) {
    return 'main.hard';
  }
  return files[0] || '';
}

function discoverEntryPoints(folder) {
  const root = folderPath(folder);
  if (!root) {
    return [];
  }
  const entries = [];
  const seen = new Set();
  const add = (absolute, source) => {
    if (!absolute || seen.has(absolute) || !regularFile(absolute)) {
      return;
    }
    seen.add(absolute);
    const relative = path.relative(root, absolute) || path.basename(absolute);
    entries.push({
      file: relative.split(path.sep).join('/'),
      label: path.basename(absolute),
      source
    });
  };

  if (regularFile(path.join(root, 'hard.toml'))) {
    const manifest = preferredHardFile(root);
    if (manifest) {
      add(path.join(root, manifest), 'hard.toml');
    }
  }

  for (const directoryName of ['example', 'examples']) {
    const directory = path.join(root, directoryName);
    if (!directoryExists(directory)) {
      continue;
    }
    for (const name of hardFilesIn(directory)) {
      add(path.join(directory, name), directoryName);
    }
    for (const child of subdirectoriesOf(directory)) {
      const nested = preferredHardFile(path.join(directory, child));
      if (nested) {
        add(path.join(directory, child, nested), directoryName);
      }
    }
  }

  if (!entries.length && !regularFile(path.join(root, 'hard.toml'))) {
    for (const name of hardFilesIn(root)) {
      add(path.join(root, name), 'workspace');
    }
  }
  return entries;
}

function taskFileArgument(definition, folder) {
  if (definition.file) {
    const configured = expandPath(String(definition.file), folder);
    const root = folderPath(folder) || process.cwd();
    const absolute = path.isAbsolute(configured) ? configured : path.join(root, configured);
    return path.relative(root, absolute) || path.basename(absolute);
  }
  return activeHardFile(folder) || 'main.hard';
}

function taskFilePath(definition, folder) {
  const root = folderPath(folder) || process.cwd();
  const candidate = definition.file
    ? expandPath(String(definition.file), folder)
    : activeHardFile(folder);
  if (!candidate) {
    return path.join(root, 'main.hard');
  }
  return path.isAbsolute(candidate) ? candidate : path.join(root, candidate);
}

function taskArguments(definition, folder) {
  const command = String(definition.command || '').toLowerCase();
  const args = [];
  if (command !== 'doctor') {
    args.push(taskFileArgument(definition, folder));
  }
  if (Array.isArray(definition.args)) {
    args.push(...definition.args.map(String));
  }
  return args;
}

function debugLaunchConfiguration(definition, folder) {
  const root = folderPath(folder) || process.cwd();
  const program = taskFilePath(definition, folder);
  const launch = {
    type: 'hardscript',
    request: 'launch',
    name: `HardScript: debug ${path.basename(program)}`,
    program,
    cwd: root,
    args: [],
    stopOnEntry: false
  };
  if (Array.isArray(definition.args)) {
    launch.args = definition.args.map(String);
  }
  if (definition.serverMode) {
    launch.serverMode = String(definition.serverMode);
  }
  if (typeof definition.serverPort === 'number' && Number.isFinite(definition.serverPort)) {
    launch.serverPort = definition.serverPort;
  }
  if (definition.stopOnEntry === true) {
    launch.stopOnEntry = true;
  }
  return launch;
}

function registerTaskProvider() {
  if (!vscode.tasks || typeof vscode.tasks.registerTaskProvider !== 'function' || !vscode.Task || !vscode.ProcessExecution) {
    return;
  }
  const provider = {
    provideTasks() {
      const tasks = [];
      for (const folder of workspaceFolders()) {
        for (const entry of discoverEntryPoints(folder)) {
          for (const command of ['build', 'run', 'test', 'format']) {
            tasks.push(new vscode.Task(
              {
                type: 'hardscript',
                command,
                file: entry.file
              },
              folder,
              `HardScript: ${command} ${entry.file}`,
              'hardscript',
              undefined,
              TASK_PROBLEM_MATCHER
            ));
          }
        }
      }
      return tasks;
    },
    resolveTask(task) {
      if (!task || !task.definition || task.definition.type !== 'hardscript') {
        return undefined;
      }
      const command = String(task.definition.command || '').toLowerCase();
      if (!TASK_COMMANDS.includes(command)) {
        return undefined;
      }
      const folder = taskFolder(task);
      const scope = task.scope || folder || (vscode.TaskScope && vscode.TaskScope.Workspace);
      if (command === 'debug') {
        if (!vscode.CustomExecution || !vscode.debug || typeof vscode.debug.startDebugging !== 'function') {
          return undefined;
        }
        const launch = debugLaunchConfiguration(task.definition, folder);
        const execution = new vscode.CustomExecution(async () => {
          const started = await vscode.debug.startDebugging(folder, launch);
          if (!started) {
            notifyError(`Could not start the HardScript debug session for ${launch.program}.`);
          }
        });
        return new vscode.Task(task.definition, scope, task.name || launch.name, 'hardscript', execution, []);
      }
      const resolved = resolveCliExecutable(folder);
      if (!resolved) {
        notifyError('Could not find hard for the HardScript task.');
        return undefined;
      }
      const args = [command, ...taskArguments(task.definition, folder)];
      const execution = new vscode.ProcessExecution(resolved.command, args, {
        cwd: folderPath(folder) || process.cwd(),
        env: process.env
      });
      return new vscode.Task(
        task.definition,
        scope,
        task.name || `HardScript: ${command}`,
        'hardscript',
        execution,
        TASK_PROBLEM_MATCHER
      );
    }
  };
  extensionContext.subscriptions.push(vscode.tasks.registerTaskProvider('hardscript', provider));
}

function debugAdapterPath() {
  const base = extensionContext ? extensionContext.extensionPath : __dirname;
  return path.join(base, 'debugAdapter', 'hardDebugAdapter.js');
}

function registerDebugSupport() {
  if (!vscode.debug) {
    return;
  }
  if (typeof vscode.debug.registerDebugConfigurationProvider === 'function') {
    const provider = {
      resolveDebugConfiguration(folder, config) {
        if (!config || config.request !== 'launch' && config.request !== 'attach') {
          return config || null;
        }
        const root = folderPath(folder) || activeWorkspaceFolder() || process.cwd();
        const resolved = Object.assign({}, config);
        if (!resolved.program) {
          const active = activeHardFile(folder);
          resolved.program = active || path.join(folderPath(folder) || root, 'main.hard');
        }
        const expanded = expandPath(String(resolved.program), folder);
        resolved.program = path.isAbsolute(expanded) ? expanded : path.resolve(root, expanded);
        if (!resolved.cwd) {
          resolved.cwd = folderPath(folder) || path.dirname(resolved.program);
        } else {
          resolved.cwd = path.resolve(expandPath(String(resolved.cwd), folder));
        }
        if (!resolved.hardPath) {
          const hard = resolveCliExecutable(folder);
          if (hard) {
            resolved.hardPath = hard.command;
          }
        }
        if (!resolved.args) {
          resolved.args = [];
        }
        return resolved;
      }
    };
    extensionContext.subscriptions.push(vscode.debug.registerDebugConfigurationProvider('hardscript', provider));
  }
  if (typeof vscode.debug.registerDebugAdapterDescriptorFactory !== 'function') {
    return;
  }
  const adapterPath = debugAdapterPath();
  const factory = {
    createDebugAdapterDescriptor() {
      return new vscode.DebugAdapterExecutable(process.execPath, [adapterPath]);
    }
  };
  extensionContext.subscriptions.push(vscode.debug.registerDebugAdapterDescriptorFactory('hardscript', factory));
}

function configurationChanged(event) {
  if (!event || typeof event.affectsConfiguration !== 'function') {
    return;
  }
  if (
    event.affectsConfiguration('hardscript.serverPath')
    || event.affectsConfiguration('hardscript.serverArgs')
    || event.affectsConfiguration('hardscript.trace.server')
  ) {
    void stopLanguageClient().then(() => startLanguageClientSafely()).catch(error => log(error));
  }
}

function workspaceFoldersChanged() {
  void stopLanguageClient().then(() => startLanguageClientSafely()).catch(error => log(error));
}

function activate(context) {
  extensionContext = context;
  outputChannel = vscode.window.createOutputChannel('HardScript');
  context.subscriptions.push(outputChannel);
  registerTaskProvider();
  registerDebugSupport();
  const registrations = [
    vscode.commands.registerCommand(COMMAND_IDS.build, () => runCliCommand('build', 'build', true)),
    vscode.commands.registerCommand(COMMAND_IDS.run, () => runCliCommand('run', 'run', true)),
    vscode.commands.registerCommand(COMMAND_IDS.test, () => runCliCommand('test', 'test', true)),
    vscode.commands.registerCommand(COMMAND_IDS.format, () => runCliCommand('format', 'fmt', true)),
    vscode.commands.registerCommand(COMMAND_IDS.doctor, () => runCliCommand('doctor', 'doctor', false)),
    vscode.commands.registerCommand(COMMAND_IDS.explainError, explainError)
  ];
  registrations.forEach(registration => context.subscriptions.push(registration));
  if (vscode.workspace && typeof vscode.workspace.onDidChangeConfiguration === 'function') {
    context.subscriptions.push(vscode.workspace.onDidChangeConfiguration(configurationChanged));
  }
  if (vscode.workspace && typeof vscode.workspace.onDidChangeWorkspaceFolders === 'function') {
    context.subscriptions.push(vscode.workspace.onDidChangeWorkspaceFolders(workspaceFoldersChanged));
  }
  void startLanguageClientSafely();
  return {
    resolveServerPath,
    resolveHardPath,
    startLanguageClient,
    stopLanguageClient
  };
}

async function deactivate() {
  await stopLanguageClient();
  for (const child of activeProcesses) {
    try {
      child.kill();
    } catch (_) {
      continue;
    }
  }
  activeProcesses.clear();
}

module.exports = {
  activate,
  deactivate,
  resolveServerPath,
  resolveHardPath,
  resolveBinary,
  discoverEntryPoints,
  debugLaunchConfiguration,
  extractDiagnosticCode: diagnosticCode,
  findDiagnosticCode
};
