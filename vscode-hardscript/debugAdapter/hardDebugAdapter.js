'use strict';

const childProcess = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const THREAD_ID = 1;
const VARIABLES_REFERENCE = 1;
const HEADER_SEPARATOR = '\r\n\r\n';
const WINDOWS = process.platform === 'win32';

let sequence = 0;
let input = Buffer.alloc(0);
let configuration = {};
let launchState = 'created';
let child = null;
let terminating = false;
let program = '';
let cwd = '';
let attached = false;
let stopLine = 0;
let stopped = false;
let breakpoints = [];

function send(message) {
    const payload = Buffer.from(JSON.stringify(message), 'utf8');
    process.stdout.write(`Content-Length: ${payload.length}${HEADER_SEPARATOR}`);
    process.stdout.write(payload);
}

function sendEvent(event, body) {
    sequence += 1;
    const message = {
        seq: sequence,
        type: 'event',
        event
    };
    if (body !== undefined) {
        message.body = body;
    }
    send(message);
}

function sendResponse(request, body) {
    sequence += 1;
    send({
        seq: sequence,
        type: 'response',
        request_seq: request.seq,
        success: true,
        command: request.command,
        body: body === undefined ? {} : body
    });
}

function sendErrorResponse(request, message) {
    sequence += 1;
    send({
        seq: sequence,
        type: 'response',
        request_seq: request.seq,
        success: false,
        command: request.command,
        message,
        body: {}
    });
}

function report(text) {
    sendEvent('output', {
        category: 'console',
        output: `hardscript debug adapter: ${text}\n`
    });
}

function isFile(candidate) {
    try {
        return fs.statSync(candidate).isFile();
    } catch (_) {
        return false;
    }
}

function isDirectory(candidate) {
    try {
        return fs.statSync(candidate).isDirectory();
    } catch (_) {
        return false;
    }
}

function binaryNames() {
    if (WINDOWS) {
        const extensions = (process.env.PATHEXT || '.EXE;.CMD;.BAT').split(';').filter(Boolean);
        const result = [];
        for (const name of ['hard', 'hard.exe']) {
            result.push(name);
        }
        for (const extension of extensions) {
            result.push(`hard${extension.toLowerCase()}`);
        }
        return result;
    }
    return ['hard'];
}

function expand(value, root) {
    let result = String(value);
    if (result === '~') {
        return os.homedir();
    }
    if (result.startsWith('~/')) {
        result = path.join(os.homedir(), result.slice(2));
    }
    if (root) {
        result = result.replace(/\$\{workspaceFolder\}/g, root);
        result = result.replace(/\$\{workspaceRoot\}/g, root);
    }
    return result;
}

function firstExisting(candidate, root) {
    if (!candidate) {
        return null;
    }
    const expanded = expand(candidate, root);
    if (path.isAbsolute(expanded)) {
        return isFile(expanded) ? expanded : null;
    }
    const resolved = path.resolve(root || process.cwd(), expanded);
    return isFile(resolved) ? resolved : null;
}

function findOnPath() {
    const environmentPath = process.env.PATH || process.env.Path || '';
    if (!environmentPath) {
        return null;
    }
    const names = binaryNames();
    for (const entry of environmentPath.split(path.delimiter).filter(Boolean)) {
        const base = path.isAbsolute(entry) ? entry : path.resolve(process.cwd(), entry);
        for (const name of names) {
            const candidate = path.resolve(base, name);
            if (isFile(candidate)) {
                return candidate;
            }
        }
    }
    return null;
}

function resolveHardBinary(root) {
    const explicit = [configuration.hardPath, configuration.cliPath, configuration.hardCliPath]
        .filter(value => typeof value === 'string' && value.trim());
    for (const candidate of explicit) {
        const resolved = firstExisting(candidate, root);
        if (resolved) {
            return { command: resolved, source: 'launch configuration' };
        }
    }
    if (process.env.HARD_BIN) {
        const resolved = firstExisting(process.env.HARD_BIN, root);
        if (resolved) {
            return { command: resolved, source: 'HARD_BIN' };
        }
    }
    const roots = [root, program ? path.dirname(program) : ''].filter(Boolean);
    const names = binaryNames();
    for (const base of roots) {
        let directory = path.resolve(base);
        for (let depth = 0; depth < 4 && directory; depth += 1) {
            for (const profile of ['release', 'debug']) {
                for (const name of names) {
                    const candidate = path.join(directory, 'target', profile, name);
                    if (isFile(candidate)) {
                        return { command: candidate, source: 'workspace target directory' };
                    }
                }
            }
            const parent = path.dirname(directory);
            if (parent === directory) {
                break;
            }
            directory = parent;
        }
    }
    const onPath = findOnPath();
    if (onPath) {
        return { command: onPath, source: 'PATH' };
    }
    return null;
}

function resolveProgram(root) {
    const raw = typeof configuration.program === 'string' ? configuration.program.trim() : '';
    if (!raw) {
        return '';
    }
    const expanded = expand(raw, root).replace(/\$\{file\}/g, root);
    return path.isAbsolute(expanded) ? expanded : path.resolve(root || process.cwd(), expanded);
}

function buildEnvironment() {
    const environment = Object.assign({}, process.env);
    if (typeof configuration.envFile === 'string' && configuration.envFile.trim()) {
        const file = path.resolve(cwd || process.cwd(), expand(configuration.envFile.trim(), cwd));
        try {
            for (const line of fs.readFileSync(file, 'utf8').split(/\r?\n/)) {
                const text = line.trim();
                if (!text || text.startsWith('#')) {
                    continue;
                }
                const index = text.indexOf('=');
                if (index <= 0) {
                    continue;
                }
                environment[text.slice(0, index).trim()] = text.slice(index + 1).trim();
            }
        } catch (error) {
            report(`could not read envFile ${file}: ${error.message}`);
        }
    }
    if (configuration.env && typeof configuration.env === 'object' && !Array.isArray(configuration.env)) {
        for (const [key, value] of Object.entries(configuration.env)) {
            environment[key] = value === null || value === undefined ? '' : String(value);
        }
    }
    return environment;
}

function buildArguments(programPath) {
    const args = ['run', programPath];
    if (Array.isArray(configuration.args)) {
        args.push(...configuration.args.map(item => String(item)));
    }
    if (configuration.serverMode !== undefined && configuration.serverMode !== null && `${configuration.serverMode}`.trim()) {
        args.push(`--serverMode=${configuration.serverMode}`);
    }
    const port = configuration.serverPort;
    if (typeof port === 'number' && Number.isFinite(port) && port >= 0) {
        args.push(`--serverPort=${port}`);
    }
    return args;
}

function markerLines() {
    return breakpoints
        .map(item => item.line)
        .filter(line => Number.isInteger(line) && line > 0)
        .sort((left, right) => left - right);
}

function nextMarkerLine(current) {
    const lines = markerLines().filter(line => line > current);
    if (lines.length) {
        return lines[0];
    }
    return current > 0 ? current : 0;
}

function emitStopped(line, reason, breakpointIds) {
    stopped = true;
    stopLine = line;
    const body = {
        reason,
        threadId: THREAD_ID
    };
    if (line > 0) {
        body.line = line;
    }
    if (Array.isArray(breakpointIds) && breakpointIds.length) {
        body.hitBreakpointIds = breakpointIds;
    }
    sendEvent('stopped', body);
}

function resume() {
    if (stopped) {
        stopped = false;
        sendEvent('continued', { threadId: THREAD_ID, allThreadsContinued: true });
    }
    return { allThreadsContinued: true };
}

function sourceReference() {
    return {
        name: program ? path.basename(program) : 'main.hard',
        path: program || 'main.hard'
    };
}

function handleInitialize(request) {
    sendResponse(request, {
        supportsConfigurationDoneRequest: true,
        supportsTerminateRequest: true,
        supportsEvaluateForHovers: false,
        supportsSetVariable: false,
        supportsRestartRequest: false,
        supportsStepBack: false,
        supportsGotoTargetsRequest: false,
        supportsCompletionsRequest: false,
        supportsLogPoints: false,
        supportsFunctionBreakpoints: false,
        supportsConditionalBreakpoints: false,
        supportsHitConditionalBreakpoints: false,
        supportsDelayedStackTraceLoading: false,
        supportTerminateDebuggee: true,
        exceptionBreakpointFilters: []
    });
    sendEvent('initialized', {});
}

function handleLaunch(request) {
    configuration = request.arguments && typeof request.arguments === 'object' ? request.arguments : {};
    const configuredCwd = typeof configuration.cwd === 'string' && configuration.cwd.trim()
        ? path.resolve(expand(configuration.cwd.trim(), process.cwd()))
        : '';
    program = resolveProgram(configuredCwd || process.cwd());
    if (!program) {
        sendErrorResponse(request, 'A launch configuration with a "program" path is required.');
        return;
    }
    cwd = isDirectory(configuredCwd) ? configuredCwd : path.dirname(program);
    const resolved = resolveHardBinary(cwd);
    if (!resolved) {
        sendErrorResponse(request, 'Could not find the hard CLI. Set "hardPath" in the launch configuration, build target/release/hard, or add hard to PATH.');
        return;
    }
    if (!isFile(program)) {
        sendErrorResponse(request, `Could not find the HardScript program ${program}.`);
        return;
    }
    launchState = 'launched';
    sendResponse(request);
    report(`${resolved.source}: ${resolved.command}`);
}

function handleSetBreakpoints(request) {
    const args = request.arguments || {};
    const requested = Array.isArray(args.breakpoints) ? args.breakpoints : [];
    const source = args.source && typeof args.source === 'object' ? args.source : {};
    const sourcePath = typeof source.path === 'string' && source.path
        ? source.path
        : program;
    breakpoints = requested.map(item => {
        const line = item && Number.isInteger(item.line) ? item.line : 0;
        return { line, id: item && item.id ? item.id : line, source: sourcePath };
    });
    sendResponse(request, {
        breakpoints: breakpoints.map(item => {
            if (item.line > 0) {
                return {
                    id: item.id,
                    verified: true,
                    line: item.line,
                    source: {
                        name: path.basename(item.source),
                        path: item.source
                    }
                };
            }
            return {
                id: item.id,
                verified: false,
                line: item.line,
                message: 'Breakpoint line must be a positive integer.',
                source: {
                    name: path.basename(item.source),
                    path: item.source
                }
            };
        })
    });
}

function handleStackTrace() {
    const frames = [{
        id: 1,
        name: 'main',
        source: sourceReference(),
        line: stopLine > 0 ? stopLine : 1,
        column: 1
    }];
    return {
        stackFrames: frames,
        totalFrames: frames.length
    };
}

function handleScopes() {
    return {
        scopes: [{
            name: 'Locals',
            variablesReference: VARIABLES_REFERENCE,
            namedVariables: 0,
            indexedVariables: 0,
            expensive: false,
            presentationHint: 'locals',
            source: sourceReference()
        }]
    };
}

function killTree(running, signal) {
    if (!running || !running.pid) {
        return;
    }
    if (WINDOWS) {
        try {
            childProcess.spawnSync('taskkill', ['/pid', String(running.pid), '/T', '/F'], { windowsHide: true });
        } catch (_) {
            return;
        }
        return;
    }
    try {
        process.kill(-running.pid, signal);
        return;
    } catch (_) {
        return;
    }
}

function stopChild() {
    const running = child;
    if (!running) {
        return Promise.resolve();
    }
    return new Promise(resolve => {
        let settled = false;
        let timer = null;
        const finish = () => {
            if (settled) {
                return;
            }
            settled = true;
            if (timer) {
                clearTimeout(timer);
            }
            child = null;
            resolve();
        };
        timer = setTimeout(() => {
            killTree(running, 'SIGKILL');
            finish();
        }, 2000);
        running.once('close', finish);
        running.once('error', finish);
        killTree(running, 'SIGTERM');
    });
}

function shutdown(code) {
    process.exitCode = code;
    try {
        process.stdin.pause();
        process.stdin.destroy();
    } catch (_) {
        process.exit(code);
        return;
    }
    try {
        process.stdout.end();
    } catch (_) {
        process.exit(code);
        return;
    }
    const timer = setTimeout(() => process.exit(code), 1000);
    timer.unref();
}

function startProgram() {
    if (launchState !== 'launched' || child) {
        return;
    }
    const resolved = resolveHardBinary(cwd);
    if (!resolved) {
        report('the hard CLI disappeared before the program could start.');
        sendEvent('terminated', {});
        return;
    }
    const args = buildArguments(program);
    launchState = 'running';
    report(`spawning ${resolved.command} ${args.join(' ')}`);
    try {
        child = childProcess.spawn(resolved.command, args, {
            cwd,
            env: buildEnvironment(),
            stdio: ['ignore', 'pipe', 'pipe'],
            detached: !WINDOWS,
            windowsHide: true
        });
    } catch (error) {
        child = null;
        report(`could not spawn the hard CLI: ${error.message}`);
        sendEvent('terminated', {});
        return;
    }
    sendEvent('process', {
        name: path.basename(program),
        systemProcessId: child.pid,
        isLocalProcess: true,
        startMethod: attached ? 'attach' : 'launch'
    });
    sendEvent('thread', { reason: 'started', threadId: THREAD_ID });
    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');
    child.stdout.on('data', chunk => sendEvent('output', { category: 'stdout', output: chunk }));
    child.stderr.on('data', chunk => sendEvent('output', { category: 'stderr', output: chunk }));
    child.once('error', error => {
        child = null;
        sendEvent('output', {
            category: 'stderr',
            output: `hardscript debug adapter: ${error.message}\n`
        });
        sendEvent('terminated', {});
    });
    child.once('close', (code, signal) => {
        child = null;
        const exitCode = typeof code === 'number' ? code : (signal ? 1 : 0);
        sendEvent('exited', { exitCode });
        if (!terminating) {
            sendEvent('terminated', {});
        }
    });
    if (configuration.stopOnEntry === true) {
        emitStopped(1, 'entry');
        return;
    }
    const first = markerLines()[0];
    if (first) {
        emitStopped(first, 'breakpoint', [first]);
    }
}

function stopAtNextMarker(reason) {
    const next = nextMarkerLine(stopLine);
    if (next > 0) {
        emitStopped(next, reason, [next]);
    } else {
        resume();
    }
}

const handlers = {
    initialize: handleInitialize,
    launch: request => {
        attached = false;
        handleLaunch(request);
    },
    attach: request => {
        attached = true;
        handleLaunch(request);
    },
    configurationDone: request => {
        sendResponse(request);
        if (configuration.noDebug !== true) {
            startProgram();
        } else {
            launchState = 'launched';
        }
    },
    setBreakpoints: request => {
        handleSetBreakpoints(request);
    },
    setFunctionBreakpoints: request => {
        sendResponse(request, { breakpoints: [] });
    },
    setExceptionBreakpoints: request => {
        sendResponse(request, { breakpoints: [] });
    },
    threads: request => {
        sendResponse(request, {
            threads: [{
                id: THREAD_ID,
                name: program ? path.basename(program) : 'HardScript'
            }]
        });
    },
    stackTrace: request => {
        sendResponse(request, handleStackTrace());
    },
    scopes: request => {
        sendResponse(request, handleScopes());
    },
    variables: request => {
        sendResponse(request, { variables: [] });
    },
    continue: request => {
        sendResponse(request, resume());
    },
    next: request => {
        sendResponse(request, {});
        stopAtNextMarker('step');
    },
    stepIn: request => {
        sendResponse(request, {});
        stopAtNextMarker('step');
    },
    stepOut: request => {
        sendResponse(request, {});
        stopAtNextMarker('step');
    },
    pause: request => {
        if (stopped) {
            sendErrorResponse(request, 'The program is already stopped at a marker line.');
            return;
        }
        sendResponse(request);
        emitStopped(stopLine > 0 ? stopLine : 1, 'pause');
    },
    evaluate: request => {
        sendResponse(request, {
            result: '',
            type: 'string',
            variablesReference: 0
        });
        report('expression evaluation is not supported; the v0.1 toolchain has no debug value protocol.');
    },
    source: request => {
        sendResponse(request, { content: '' });
    },
    terminate: request => {
        terminating = true;
        sendResponse(request);
        stopChild().then(() => {
            sendEvent('terminated', {});
        });
    },
    disconnect: request => {
        terminating = true;
        sendResponse(request);
        stopChild().then(() => {
            sendEvent('terminated', {});
            shutdown(0);
        });
    },
    restart: request => {
        sendErrorResponse(request, 'Restart is not supported by the HardScript debug adapter.');
    }
};

function handleMessage(text) {
    let message;
    try {
        message = JSON.parse(text);
    } catch (error) {
        report(`could not parse a protocol message: ${error.message}`);
        return;
    }
    if (!message || message.type !== 'request') {
        return;
    }
    const handler = handlers[message.command];
    if (!handler) {
        sendErrorResponse(message, `Unsupported request ${message.command}.`);
        return;
    }
    try {
        handler(message);
    } catch (error) {
        sendErrorResponse(message, error && error.message ? error.message : String(error));
    }
}

function consume(chunk) {
    input = Buffer.concat([input, chunk]);
    for (;;) {
        const headerEnd = input.indexOf(HEADER_SEPARATOR);
        if (headerEnd < 0) {
            return;
        }
        const header = input.slice(0, headerEnd).toString('ascii');
        const match = /content-length:\s*(\d+)/i.exec(header);
        if (!match) {
            input = input.slice(headerEnd + HEADER_SEPARATOR.length);
            continue;
        }
        const length = Number.parseInt(match[1], 10);
        const start = headerEnd + HEADER_SEPARATOR.length;
        if (!Number.isFinite(length) || input.length < start + length) {
            return;
        }
        const body = input.slice(start, start + length).toString('utf8');
        input = input.slice(start + length);
        handleMessage(body);
    }
}

function main() {
    process.stdin.on('data', chunk => consume(chunk));
    process.stdin.on('end', () => {
        terminating = true;
        stopChild().then(() => shutdown(0));
    });
    process.stdin.resume();
    process.on('exit', () => {
        if (child) {
            killTree(child, 'SIGKILL');
        }
    });
    process.on('SIGINT', () => {
        terminating = true;
        stopChild().then(() => shutdown(0));
    });
    process.on('SIGTERM', () => {
        terminating = true;
        stopChild().then(() => shutdown(0));
    });
}

main();
