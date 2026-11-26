#!/usr/bin/env node
// Tauri CLI 转交器：cargo 常常不在 GUI / 普通 shell 的 PATH 里（本机就是这种情况），
// 直接跑 tauri CLI 会报 `failed to run cargo metadata ... No such file or directory`。
// 这里先探测 Rust 工具链，把它的 bin 目录注入 PATH，再原样转交参数给 @tauri-apps/cli。
import { spawn } from 'node:child_process'
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs'
import { homedir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const home = process.env.HOME || homedir()

const isWin = process.platform === 'win32'

function isExec(file) {
  try {
    const st = statSync(file)
    if (!st.isFile()) return false
    // Windows 上可执行位不可靠，能读到文件就算可执行
    return isWin || (st.mode & 0o111) !== 0
  } catch {
    return false
  }
}

function toolchainBins() {
  const out = []
  const rustup = path.join(process.env.RUSTUP_HOME || path.join(home, '.rustup'), 'toolchains')
  try {
    const dirs = readdirSync(rustup)
      .filter((name) => existsSync(path.join(rustup, name, 'bin', 'cargo')))
      .sort((a, b) => b.localeCompare(a, 'en'))
    for (const name of dirs) out.push(path.join(rustup, name, 'bin'))
  } catch {
    // 没有 rustup 目录就跳过
  }
  return out
}

function findCargo() {
  const candidates = []
  const exe = isWin ? 'cargo.exe' : 'cargo'
  if (process.env.MONITOR_CARGO) candidates.push(process.env.MONITOR_CARGO)
  for (const dir of (process.env.PATH || '').split(path.delimiter)) {
    if (dir) candidates.push(path.join(dir, exe))
  }
  if (process.env.CARGO_HOME) candidates.push(path.join(process.env.CARGO_HOME, 'bin', exe))
  candidates.push(path.join(home, '.cargo', 'bin', exe))
  candidates.push(path.join(root, '.rust', 'cargo', 'bin', exe))
  for (const bin of toolchainBins()) candidates.push(path.join(bin, exe))
  for (const file of candidates) {
    if (file && isExec(file)) return file
  }
  return null
}

const cargo = findCargo()
if (!cargo) {
  console.error('未找到 Rust 工具链（cargo）。请先安装：')
  console.error('  curl --proto \'=https\' --tlsv1.2 -sSf https://sh.rustup.rs | sh')
  console.error('或: brew install rust')
  process.exit(1)
}

const binDir = path.dirname(cargo)
const env = { ...process.env }
env.PATH = binDir + path.delimiter + (env.PATH || '')
if (!env.RUSTUP_HOME && existsSync(path.join(home, '.rustup'))) {
  env.RUSTUP_HOME = path.join(home, '.rustup')
}
if (!env.CARGO_HOME) {
  const localCargo = path.join(root, '.rust', 'cargo')
  if (existsSync(localCargo)) env.CARGO_HOME = localCargo
}
if (process.env.MONITOR_PRINT_TOOLCHAIN) {
  console.error('[monitor] cargo: ' + cargo)
  console.error('[monitor] PATH+: ' + binDir)
}

// 直接跑 @tauri-apps/cli 的入口脚本：node_modules/.bin 在 Windows 上是 .cmd，
// 用 spawn 直接执行会失败（ENOENT），统一交给当前 node 进程执行最稳。
const cliPkg = path.join(root, 'node_modules', '@tauri-apps', 'cli', 'package.json')
let cli = path.join(root, 'node_modules', '.bin', 'tauri')
if (existsSync(cliPkg)) {
  try {
    const pkg = JSON.parse(readFileSync(cliPkg, 'utf8'))
    const bin = typeof pkg.bin === 'string' ? pkg.bin : pkg.bin && pkg.bin.tauri
    if (bin) cli = path.join(path.dirname(cliPkg), bin)
  } catch {
    // package.json 读不出来就退回 .bin/tauri
  }
}
if (!existsSync(cli)) {
  console.error('未找到 Tauri CLI，请先安装依赖：pnpm install')
  process.exit(1)
}

const args = process.argv.slice(2)
const child = spawn(process.execPath, [cli, ...args], { env, stdio: 'inherit' })
child.on('error', (err) => {
  console.error('无法启动 Tauri CLI：' + err.message)
  process.exit(1)
})
child.on('exit', (code, signal) => {
  if (signal) process.kill(process.pid, signal)
  else process.exit(code === null ? 1 : code)
})
