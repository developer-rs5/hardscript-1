'use strict';

const childProcess = require('child_process');
const fs = require('fs');
const path = require('path');

const projectRoot = path.resolve(__dirname, '..');
const skippedDirectories = new Set([
  '.git',
  '.vscode-test',
  '.vscode-test-web',
  'dist',
  'node_modules',
  'out',
  'target'
]);
const failures = [];
const checked = {
    js: [],
    json: []
};

function collect(directory) {
    let entries;
    try {
        entries = fs.readdirSync(directory, { withFileTypes: true });
    } catch (_) {
        return;
    }
    for (const entry of entries) {
        const absolute = path.join(directory, entry.name);
        if (entry.isDirectory()) {
            if (!skippedDirectories.has(entry.name)) {
                collect(absolute);
            }
            continue;
        }
        if (!entry.isFile()) {
            continue;
        }
        if (entry.name.endsWith('.js')) {
            checkJavaScript(absolute);
        } else if (entry.name.endsWith('.json')) {
            checkJson(absolute);
        }
    }
}

function relative(file) {
    return path.relative(projectRoot, file).split(path.sep).join('/');
}

function checkJavaScript(file) {
    checked.js.push(relative(file));
    const result = childProcess.spawnSync(process.execPath, ['--check', file], { encoding: 'utf8' });
    if (result.error) {
        failures.push(`${relative(file)}: ${result.error.message}`);
        return;
    }
    if (result.status !== 0) {
        const output = `${result.stderr || ''}${result.stdout || ''}`.trim();
        failures.push(`${relative(file)}: ${output || 'node --check failed'}`);
    }
}

function checkJson(file) {
    checked.json.push(relative(file));
    let text;
    try {
        text = fs.readFileSync(file, 'utf8');
    } catch (error) {
        failures.push(`${relative(file)}: ${error.message}`);
        return;
    }
    try {
        JSON.parse(text.replace(/^\uFEFF/, ''));
    } catch (error) {
        failures.push(`${relative(file)}: ${error.message}`);
    }
}

collect(projectRoot);

for (const failure of failures) {
    process.stdout.write(`FAIL ${failure}\n`);
}
process.stdout.write(`checked ${checked.js.length} JavaScript files and ${checked.json.length} JSON files\n`);
if (failures.length) {
    process.stdout.write(`${failures.length} failure(s)\n`);
    process.exitCode = 1;
}
