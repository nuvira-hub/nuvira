#!/bin/sh
# Runs inside a Debian 12 container (tools/publish-repo.sh): adds the .deb and
# .rpm in /in to the package repositories in /repo, keeps the last 3 versions,
# and signs them with the key in /key/repo-signing.asc.
#
#   /repo/apt   apt: pool/main/*.deb, dists/stable/{InRelease,Release,Release.gpg}
#   /repo/rpm   dnf: packages/*.rpm (each signed), repodata/ (repomd.xml signed)
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null
apt-get install -y -qq apt-utils gnupg createrepo-c rpm >/dev/null

export GNUPGHOME="$(mktemp -d)"
gpg --batch --quiet --import /key/repo-signing.asc
FP="$(gpg --list-secret-keys --with-colons | awk -F: '/^fpr/{print $10; exit}')"
sign() { gpg --batch --yes --pinentry-mode loopback --passphrase '' -u "$FP" "$@"; }
KEEP=3

# apt
mkdir -p /repo/apt/pool/main /repo/apt/dists/stable/main/binary-amd64
for deb in /in/*.deb; do
    v="$(dpkg-deb -f "$deb" Version)"
    cp "$deb" "/repo/apt/pool/main/nuvira_${v}_amd64.deb"
done
ls /repo/apt/pool/main/nuvira_*_amd64.deb | sort -V | head -n -"$KEEP" | xargs -r rm -f
cd /repo/apt
apt-ftparchive packages pool > dists/stable/main/binary-amd64/Packages
gzip -9kf dists/stable/main/binary-amd64/Packages
apt-ftparchive \
    -o APT::FTPArchive::Release::Origin=NuviraHub \
    -o APT::FTPArchive::Release::Label=NuviraHub \
    -o APT::FTPArchive::Release::Suite=stable \
    -o APT::FTPArchive::Release::Codename=stable \
    -o APT::FTPArchive::Release::Architectures=amd64 \
    -o APT::FTPArchive::Release::Components=main \
    -o APT::FTPArchive::Release::Description="NuviraHub remote desktop" \
    release dists/stable > dists/stable/Release.new
mv dists/stable/Release.new dists/stable/Release
sign --clearsign -o dists/stable/InRelease dists/stable/Release
sign --armor --detach-sign -o dists/stable/Release.gpg dists/stable/Release
cp /key/nuvirahub.gpg /key/nuvirahub.asc /key/nuvirahub.sources /repo/apt/

# dnf
mkdir -p /repo/rpm/packages
cat > "$HOME/.rpmmacros" <<EOF
%_gpg_name $FP
%__gpg /usr/bin/gpg
%_gpg_sign_cmd_extra_args --pinentry-mode loopback --passphrase ''
EOF
for rpm in /in/*.rpm; do
    v="$(rpm -qp --qf '%{VERSION}-%{RELEASE}' "$rpm" 2>/dev/null)"
    out="/repo/rpm/packages/nuvira-${v}.x86_64.rpm"
    cp "$rpm" "$out"
    rpmsign --addsign "$out" >/dev/null
done
ls /repo/rpm/packages/nuvira-*.x86_64.rpm | sort -V | head -n -"$KEEP" | xargs -r rm -f
createrepo_c --quiet --update /repo/rpm
sign --armor --detach-sign -o /repo/rpm/repodata/repomd.xml.asc /repo/rpm/repodata/repomd.xml
cp /key/nuvirahub.asc /key/nuvirahub.repo /repo/rpm/

rm -rf "$GNUPGHOME"
echo "== apt: $(ls /repo/apt/pool/main | tr '\n' ' ')"
echo "== rpm: $(ls /repo/rpm/packages | tr '\n' ' ')"
