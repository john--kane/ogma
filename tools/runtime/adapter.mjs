import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
const require = createRequire(import.meta.url);
const home = path.dirname(fileURLToPath(import.meta.url));
const chunks = [];
let size = 0;
for await (const chunk of process.stdin) {
  size += chunk.length;
  if (size > 8 * 1024 * 1024) throw new Error('request exceeds frame limit');
  chunks.push(chunk);
}
const request = JSON.parse(Buffer.concat(chunks).toString('utf8'));
const response = { protocolVersion: request.protocolVersion, id: request.id, status: 'ok', diagnostics: [], output: [], trace: [], result: null };
let outputBytes = 0;
function output(text, stream = 'stdout') {
  text = String(text);
  outputBytes += Buffer.byteLength(text) + 64;
  if (outputBytes > request.limits.maxOutputBytes) throw new Error('tool output limit exceeded');
  response.output.push({ stream, text });
}
function diagnostic(code, message, specifier = '', line = 0, column = 0) {
  if (response.diagnostics.length >= 200) return;
  response.diagnostics.push({ code, message: String(message).slice(0, 4096), specifier, line, column });
  response.status = 'error';
}
function atomicWrite(file, bytes) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const temporary = `${file}.ogma-${process.pid}.tmp`;
  try {
    fs.writeFileSync(temporary, bytes, { flag: 'wx', mode: 0o600 });
    const fd = fs.openSync(temporary, 'r');
    try { fs.fsyncSync(fd); } finally { fs.closeSync(fd); }
    fs.renameSync(temporary, file);
  } finally {
    try { fs.unlinkSync(temporary); } catch (error) { if (error.code !== 'ENOENT') throw error; }
  }
}
async function bundle() {
  const { rolldown } = await import('rolldown');
  const target = path.resolve(request.tool.output);
  if (target === path.resolve(request.entry)) throw new Error('bundle output must differ from its entry');
  const build = await rolldown({ input: path.resolve(request.entry), platform: 'neutral',
    onwarn(warning) { output(warning.message, 'stderr'); } });
  try {
    const generated = await build.generate({ file: target, format: 'esm', sourcemap: true, inlineDynamicImports: true });
    const chunk = generated.output.find(item => item.type === 'chunk');
    if (!chunk || generated.output.filter(item => item.type === 'chunk').length !== 1)
      throw new Error('MVP bundler requires a single ESM output');
    for (const asset of generated.output.filter(item => item.type === 'asset')) {
      if (asset.fileName !== path.basename(target) + '.map') throw new Error('unsupported bundle asset');
      atomicWrite(target + '.map', asset.source);
    }
    atomicWrite(target, chunk.code);
    output(`bundle: ${target}`);
  } finally { await build.close(); }
}
async function check() {
  const ts = require('typescript');
  const target = path.resolve(request.entry);
  const isDirectory = fs.existsSync(target) && fs.statSync(target).isDirectory();
  const defaults = { target: ts.ScriptTarget.ESNext, module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler, strict: true, noEmit: true,
    allowImportingTsExtensions: true, allowJs: true, moduleDetection: ts.ModuleDetectionKind.Force, lib: ['lib.esnext.d.ts'] };
  const privateTypes = path.join(home, 'types');
  const privateCompiler = path.dirname(require.resolve('typescript'));
  const grants = [...request.permissions.read, privateTypes, privateCompiler].map(file => {
    const real = fs.realpathSync(file);
    return { real, directory: fs.statSync(real).isDirectory() };
  });
  function allowed(file) {
    let real;
    try { real = fs.realpathSync(file); } catch { return false; }
    return grants.some(grant => grant.directory
      ? real === grant.real || (!path.relative(grant.real, real).startsWith('..' + path.sep) && path.relative(grant.real, real) !== '..' && !path.isAbsolute(path.relative(grant.real, real)))
      : real === grant.real);
  }
  const system = { ...ts.sys,
    readFile: file => allowed(file) ? ts.sys.readFile(file) : undefined,
    fileExists: file => allowed(file) && ts.sys.fileExists(file),
    readDirectory: (file, ...args) => allowed(file) ? ts.sys.readDirectory(file, ...args).filter(allowed) : [],
    directoryExists: file => allowed(file) && ts.sys.directoryExists(file),
  };
  let roots, options = defaults;
  if (isDirectory || path.basename(target) === 'tsconfig.json') {
    const config = isDirectory ? ts.findConfigFile(target, system.fileExists) : target;
    if (config) {
      const read = ts.readConfigFile(config, system.readFile);
      if (read.error) report(read.error, ts);
      const parsed = ts.parseJsonConfigFileContent(read.config ?? {}, system, path.dirname(config), defaults, config);
      for (const error of parsed.errors) report(error, ts);
      roots = parsed.fileNames;
      options = { ...parsed.options, noEmit: true };
    } else {
      roots = system.readDirectory(target, ['.ts', '.mts', '.js', '.mjs'], ['**/node_modules/**', '**/.ogma/**', '**/.git/**']);
    }
  } else roots = [target];
  if (roots.length > 4096) throw new Error('check exceeds 4096 source files');
  if (roots.length === 0) throw new Error('no source files to check');
  // Scaffolds carry the generated ambient declarations. Use those once rather
  // than injecting a second copy of the same global constants.
  const localTypes = path.join(isDirectory ? target : path.dirname(target), 'ogma.d.ts');
  const runtimeTypes = system.fileExists(localTypes) ? localTypes : path.join(privateTypes, 'ogma.d.ts');
  if (!roots.includes(runtimeTypes)) roots.push(runtimeTypes);
  const host = ts.createCompilerHost(options);
  host.readFile = system.readFile;
  host.fileExists = system.fileExists;
  host.directoryExists = system.directoryExists;
  host.readDirectory = system.readDirectory;
  if (request.source !== '') {
    const read = host.readFile;
    const exists = host.fileExists;
    host.readFile = file => path.resolve(file) === target ? request.source : read(file);
    host.fileExists = file => path.resolve(file) === target || exists(file);
  }
  const program = ts.createProgram(roots, options, host);
  for (const error of ts.getPreEmitDiagnostics(program)) report(error, ts);
  if (response.status === 'ok') output(`check: ${roots.filter(file => !file.endsWith('.d.ts')).length} source file(s)`);
}
function report(error, ts) {
  const location = error.file && error.start !== undefined ? error.file.getLineAndCharacterOfPosition(error.start) : null;
  diagnostic(`OGMA_TS${error.code}`, ts.flattenDiagnosticMessageText(error.messageText, '\n'),
    error.file?.fileName ?? '', location ? location.line + 1 : 0, location ? location.character + 1 : 0);
}
try {
  if (request.command === 'bundle') await bundle();
  else if (request.command === 'check') await check();
  else throw new Error('unknown tool adapter');
} catch (error) {
  diagnostic('OGMA_TOOLING', error?.message ?? error);
}
process.stdout.write(JSON.stringify(response));
