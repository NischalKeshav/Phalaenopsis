#!/usr/bin/env bash
# Downloads the public web projects the eval runs against into eval/sites/.
# Static files only: nothing here is built or installed, and no repo scripts run.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p sites/repos sites/downloads

# MDN learning-area, forms section only (sparse, shallow).
if [ ! -d sites/repos/mdn-learning-area ]; then
  git clone -q --depth 1 --filter=blob:none --sparse https://github.com/mdn/learning-area.git sites/repos/mdn-learning-area
  git -C sites/repos/mdn-learning-area sparse-checkout set html/forms
fi

# jquery-validation 1.19.5: the last release that ships its demo/ forms.
if [ ! -d sites/repos/jquery-validation ]; then
  git -c advice.detachedHead=false clone -q --depth 1 --branch 1.19.5 https://github.com/jquery-validation/jquery-validation.git sites/repos/jquery-validation
fi

# Bootstrap 5.3.3 official examples bundle (static HTML: checkout, sign-in, ...).
if [ ! -d sites/bootstrap-examples ]; then
  curl -sSL -o sites/downloads/bootstrap-examples.zip https://github.com/twbs/bootstrap/releases/download/v5.3.3/bootstrap-5.3.3-examples.zip
  mkdir -p sites/bootstrap-examples
  unzip -q -o sites/downloads/bootstrap-examples.zip -d sites/bootstrap-examples
fi
echo "sites ready in eval/sites"
