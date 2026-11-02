import { readFileSync, writeFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';

function git(args, { allowFailure = false } = {}) {
  const result = spawnSync('git', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
  if (result.error) throw result.error;
  if (!allowFailure && result.status !== 0) {
    throw new Error(result.stderr.trim() || `git ${args.join(' ')} failed`);
  }
  return { status: result.status, stdout: result.stdout.trim(), stderr: result.stderr.trim() };
}

function fail(message) {
  console.error(`Release not started: ${message}`);
  process.exit(1);
}

try {
  const bump = process.argv[2];
  if (!bump) fail('choose patch, minor, major, or provide a version such as 1.2.3.');

  const packageJson = JSON.parse(readFileSync('package.json', 'utf8'));
  const currentVersion = packageJson.version;
  const match = currentVersion.match(/^(\d+)\.(\d+)\.(\d+)$/);
  if (!match) fail(`current version ${currentVersion} is not a stable x.y.z version.`);

  let nextVersion;
  if (['patch', 'minor', 'major'].includes(bump)) {
    let [major, minor, patch] = match.slice(1).map(Number);
    if (bump === 'major') [major, minor, patch] = [major + 1, 0, 0];
    if (bump === 'minor') [major, minor, patch] = [major, minor + 1, 0];
    if (bump === 'patch') patch += 1;
    nextVersion = `${major}.${minor}.${patch}`;
  } else if (/^\d+\.\d+\.\d+$/.test(bump)) {
    nextVersion = bump;
    const requested = nextVersion.split('.').map(Number);
    const current = match.slice(1).map(Number);
    const isGreater = requested.some((part, index) => part > current[index] && requested.slice(0, index).every((value, before) => value === current[before]));
    if (!isGreater) fail(`the requested version must be greater than ${currentVersion}.`);
  } else {
    fail('use patch, minor, major, or a version such as 1.2.3.');
  }

  const branch = git(['branch', '--show-current']).stdout;
  if (!branch) fail('release from a named branch, not detached HEAD.');
  const upstream = git(['rev-parse', '--abbrev-ref', '--symbolic-full-name', '@{upstream}'], { allowFailure: true });
  if (upstream.status !== 0) fail('this branch needs a pushed upstream before releasing.');
  if (git(['status', '--porcelain']).stdout) fail('commit or discard your changes before releasing.');

  const slash = upstream.stdout.indexOf('/');
  const remote = upstream.stdout.slice(0, slash);
  const remoteBranch = upstream.stdout.slice(slash + 1);
  if (slash < 1 || !remoteBranch) fail(`could not read the upstream branch: ${upstream.stdout}`);
  git(['fetch', '--no-tags', remote, remoteBranch]);
  const counts = git(['rev-list', '--left-right', '--count', `HEAD...${upstream.stdout}`]).stdout.split(/\s+/).map(Number);
  if (counts[0] !== 0 || counts[1] !== 0) {
    fail(`push or pull your branch first; it is ${counts[0]} commit(s) ahead and ${counts[1]} behind ${upstream.stdout}.`);
  }

  const tag = `v${nextVersion}`;
  if (git(['show-ref', '--verify', '--quiet', `refs/tags/${tag}`], { allowFailure: true }).status === 0) {
    fail(`tag ${tag} already exists locally.`);
  }
  const remoteTag = git(['ls-remote', '--exit-code', '--tags', remote, `refs/tags/${tag}`], { allowFailure: true });
  if (remoteTag.status === 0) fail(`tag ${tag} already exists on ${remote}.`);
  if (remoteTag.status !== 2) fail(`could not check whether ${tag} already exists on ${remote}: ${remoteTag.stderr}`);

  const lock = JSON.parse(readFileSync('package-lock.json', 'utf8'));
  packageJson.version = nextVersion;
  lock.version = nextVersion;
  if (lock.packages?.['']) lock.packages[''].version = nextVersion;
  const tauriConfig = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8'));
  tauriConfig.version = nextVersion;

  writeFileSync('package.json', `${JSON.stringify(packageJson, null, 2)}\n`);
  writeFileSync('package-lock.json', `${JSON.stringify(lock, null, 2)}\n`);
  writeFileSync('src-tauri/tauri.conf.json', `${JSON.stringify(tauriConfig, null, 2)}\n`);

  const cargoToml = readFileSync('src-tauri/Cargo.toml', 'utf8');
  const updatedCargoToml = cargoToml.replace(
    /(^\[package\][\s\S]*?^version = ")[^"]+("\s*$)/m,
    (_match, prefix, suffix) => `${prefix}${nextVersion}${suffix}`,
  );
  if (updatedCargoToml === cargoToml) fail('could not update the package version in src-tauri/Cargo.toml.');
  writeFileSync('src-tauri/Cargo.toml', updatedCargoToml);

  const cargoLock = readFileSync('src-tauri/Cargo.lock', 'utf8');
  const updatedCargoLock = cargoLock.replace(
    /(^\[\[package\]\]\nname = "invoke-tool-suite"\nversion = ")[^"]+("$)/m,
    (_match, prefix, suffix) => `${prefix}${nextVersion}${suffix}`,
  );
  if (updatedCargoLock === cargoLock) fail('could not update the package version in src-tauri/Cargo.lock.');
  writeFileSync('src-tauri/Cargo.lock', updatedCargoLock);

  git(['add', 'package.json', 'package-lock.json', 'src-tauri/Cargo.toml', 'src-tauri/Cargo.lock', 'src-tauri/tauri.conf.json']);
  git(['commit', '-m', `chore: release ${tag}`]);
  git(['tag', '-a', tag, '-m', `Invoke Tool Suite ${tag}`]);
  git(['push', '--atomic', remote, `HEAD:refs/heads/${remoteBranch}`, `refs/tags/${tag}`]);

  console.log(`Pushed ${tag}. GitHub Actions will build and publish the release.`);
} catch (error) {
  console.error(`Release failed: ${error.message}`);
  process.exit(1);
}
