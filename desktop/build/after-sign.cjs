const { execFileSync } = require('child_process');
const path = require('path');

exports.default = async function afterSign(context) {
  if (process.env.CSC_NAME) return;
  if (process.platform !== 'darwin') return;
  const appPath = path.join(context.appOutDir, `${context.packager.appInfo.productFilename}.app`);
  execFileSync('codesign', ['--force', '--deep', '-s', '-', appPath], { stdio: 'inherit' });
};
