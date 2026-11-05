require "fileutils"
require "open3"
require "tmpdir"
require "yaml"

root = File.expand_path("..", __dir__)
pages = YAML.safe_load(File.read(File.join(root, ".github/workflows/pages.yml")), aliases: true)
scan = pages.fetch("jobs").fetch("build").fetch("steps")
  .find { |step| step["name"].to_s.match?(/Check .* for private text/) }.fetch("run")

def check(scan, files, expected, create_site: true)
  Dir.mktmpdir("acan-pages-scan") do |dir|
    FileUtils.mkdir_p(File.join(dir, "_site/replay")) if create_site
    files.each do |name, body|
      path = File.join(dir, "_site", name)
      FileUtils.mkdir_p(File.dirname(path))
      File.binwrite(path, body)
    end
    output, status = Open3.capture2e("bash", "-e", "-c", scan, chdir: dir)
    raise "unexpected Pages scan result for #{files.keys}: #{output}" unless status.success? == expected
    files.each_value do |body|
      raise "Pages scan printed private content" if !expected && output.include?(body)
    end
  end
end

samples = [
  "ghp_" + "A" * 36,
  "gho_" + "B" * 36,
  "github_pat_" + "C" * 82,
  "-----BEGIN PRIVATE KEY-----",
  "-----BEGIN RSA PRIVATE KEY-----",
  "-----BEGIN OPENSSH PRIVATE KEY-----",
  "AKIA" + "D" * 16,
  "ASIA" + "E" * 16,
  "sk-proj-" + "F" * 32,
  "acan_" + "a1" * 16,
  "acan_agent_" + "b2" * 16,
  "Bearer " + "G" * 24,
  "bearer\t" + "H" * 24,
  "xoxb-" + "1" * 24,
  "/home/alice/project",
  "/Users/alice/project",
  "/root/project",
  "/tmp/private",
  "/private/var/folders/private",
  "/var/folders/ab/private",
  "/var/tmp/private",
  "C:\\Users\\alice\\project",
  'C:\\\\Users\\\\alice\\\\project',
  "D:/work/private",
  "\\\\server\\share\\private",
]
samples.each do |sample|
  check(scan, { "replay/events.json" => sample }, false)
  check(scan, { "assets/app.js" => sample }, false)
end
check(scan, { ".hidden" => samples.first }, false)
check(scan, { "pkg/module.wasm" => "\0binary\0" + samples.first }, false)
check(scan, { "index.html" => "<h1>Replay</h1>", "replay/events.json" => "[]" }, true)
check(scan, {}, false, create_site: false)

Dir.mktmpdir("acan-pages-error") do |dir|
  File.write(File.join(dir, "grep"), "#!/bin/sh\nexit 2\n")
  File.chmod(0o700, File.join(dir, "grep"))
  output, status = Open3.capture2e({ "PATH" => "#{dir}:#{ENV.fetch("PATH")}" }, "bash", "-e", "-c", scan)
  raise "a scanner error must fail closed: #{output}" if status.success?
end

assets = Dir.glob(File.join(root, "crates/acan-render-web/www/**/*"), File::FNM_DOTMATCH)
  .select { |path| File.file?(path) }
  .to_h { |path| [path.sub(File.join(root, "crates/acan-render-web/www/"), ""), File.binread(path)] }
check(scan, assets.merge("replay/events.json" => "[]"), true)
puts "Pages scan: #{samples.length} secret/path formats blocked in replay and assets; binary, hidden and error cases passed."
