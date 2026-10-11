// Run against the real CLI, without credentials or network calls.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const root = process.argv[2] ?? '/repo';
const source = readFileSync(resolve(root, 'src/github.rs'), 'utf8');
const production = source.split('#[cfg(test)]')[0];
const fields = source.match(/^const PR_FIELDS: &str = "([^"]+)";/m)?.[1];
assert.ok(fields, 'Cannot find PR_FIELDS in src/github.rs');

const version = execFileSync('gh', ['--version'], { encoding: 'utf8' });
assert.match(version, /^gh version 2\.102\.0\s/, 'The pinned gh version must be installed');
console.log(version.split('\n')[0]);

const commands = new Map([
  ['list', ['--state', 'open', '--head', 'helm/probe', '--base', 'main', '--limit', '100', '--json', fields]],
  ['create', ['--head', 'helm/probe', '--base', 'main', '--title', 'Compatibility probe', '--body-file', '-']],
  ['view', ['1', '--json', fields]],
  ['ready', ['1']],
  ['merge', ['1', '--squash', '--match-head-commit', '0'.repeat(40)]],
]);

const sourceCommands = new Set([...production.matchAll(/\.run\(\s*cwd,\s*"([^"]+)"/g)].map((match) => match[1]));
assert.deepEqual([...commands.keys()].sort(), [...sourceCommands].sort(), 'Every source command must be checked');
const sourceFlags = new Set([...production.matchAll(/"(--[a-z][a-z-]*)"/g)].map((match) => match[1]));
const checkedFlags = new Set(['--repo', ...[...commands.values()].flat().filter((option) => option.startsWith('--'))]);
assert.deepEqual([...checkedFlags].sort(), [...sourceFlags].sort(), 'Every source option must be checked');

for (const [command, options] of commands) {
  const args = ['pr', command, '--repo', 'github.com/charlescstpierr/helm-sandbox', ...options, '--help'];
  const help = execFileSync('gh', args, { encoding: 'utf8' });
  const normalized = help.replace(/\s+/g, ' ');
  assert.ok(normalized.includes(`gh pr ${command}`), `Missing command help: ${command}`);
  if (options.includes('--json')) {
    const jsonFields = help.split('JSON FIELDS')[1]?.split('EXAMPLES')[0];
    assert.ok(jsonFields, `Missing JSON fields: ${command}`);
    for (const field of fields.split(',')) {
      assert.match(jsonFields, new RegExp(`\\b${field}\\b`), `Missing field: ${command}.${field}`);
    }
  }
  if (command === 'create') {
    assert.ok(normalized.includes('use "-" to read from standard input'), 'Missing stdin support for --body-file');
  }
  const flags = ['--repo', ...options.filter((option) => option.startsWith('--'))];
  console.log(`PASS gh pr ${command}: ${flags.join(', ')}`);
}
console.log(`PASS JSON fields for list/view: ${fields}`);
console.log('Only --help was executed for PR commands; no GitHub mutation or authentication tested.');
