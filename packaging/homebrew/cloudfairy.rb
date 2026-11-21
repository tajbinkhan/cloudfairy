cask "cloudfairy" do
  version "@VERSION@"
  sha256 "@DMG_SHA256@"

  url "https://github.com/tajbinkhan/cloudfairy/releases/download/v#{version}/Cloudfairy_#{version}_universal.dmg"
  name "Cloudfairy"
  desc "Share a local port to the internet with Cloudflare quick tunnels"
  homepage "https://tajbinkhan.github.io/cloudfairy/"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates true

  app "Cloudfairy.app"

  zap trash: [
    "~/Library/Application Support/dev.cloudfairy.desktop",
    "~/Library/Caches/dev.cloudfairy.desktop",
    "~/Library/LaunchAgents/Cloudfairy.plist",
    "~/Library/WebKit/dev.cloudfairy.desktop",
  ]
end
