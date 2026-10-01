import { execFileSync } from 'node:child_process';
import path from 'node:path';

/**
 * electron-builder `afterPack` hook: ad-hoc (re)signing for the unsigned macOS build.
 *
 * `identity: null` skips electron-builder's signing step, but by the time this hook
 * runs electron-builder has already rewritten the bundle (Info.plist, app.asar).
 * That invalidates the ad-hoc signature Electron ships:
 *
 * ```
 * $ codesign --verify --verbose=2 release/mac-arm64/Wing.app
 * Wing.app: code has no resources but signature indicates they must be present
 * ```
 *
 * The app still launches from the build directory, but as soon as the bundle
 * carries a quarantine attribute (downloaded / copied around) macOS refuses it as
 * damaged. Signing the finished bundle with `--sign -` (ad-hoc, no identity, no
 * notarization — both explicitly out of scope) fixes that in ~0.5 s: the code
 * directory gets `Identifier=com.wing-agent.app` and seals the resources, so
 * `codesign --verify` reports "valid on disk" / "satisfies its Designated
 * Requirement".
 *
 * `--deep` is deprecated for real signing but is the standard way to ad-hoc sign
 * an Electron bundle in place (it walks the nested helpers/frameworks in the right
 * order). macOS only: no other platform has `codesign`, and none is packaged here.
 */
export default function afterPack(context) {
  if (context.electronPlatformName !== 'darwin') {
    return;
  }
  const appBundle = path.join(context.appOutDir, `${context.packager.appInfo.productFilename}.app`);
  process.stderr.write(`[after-pack] ad-hoc signing ${appBundle}\n`);
  execFileSync('codesign', ['--force', '--deep', '--sign', '-', '--timestamp=none', appBundle], {
    stdio: 'inherit',
  });
}
