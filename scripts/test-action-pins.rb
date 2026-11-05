require "fileutils"
require "open3"
require "tmpdir"
require "yaml"

root = File.expand_path("..", __dir__)
ci = YAML.safe_load(File.read(File.join(root, ".github/workflows/ci.yml")), aliases: true)
guard = ci.fetch("jobs").fetch("fmt").fetch("steps")
  .find { |step| step["name"] == "Check action pins" }.fetch("run")

def check(guard, files, expected)
  Dir.mktmpdir("acan-action-pins") do |dir|
    FileUtils.mkdir_p(File.join(dir, ".github/workflows"))
    files.each { |name, body| File.write(File.join(dir, ".github/workflows", name), body) }
    output, status = Open3.capture2e("bash", "-e", "-c", guard, chdir: dir)
    raise "unexpected action-pin result: #{output}" unless status.success? == expected
  end
end

sha = "a" * 40
check(guard, { "ci.yml" => "jobs:\n  test:\n    steps:\n      - uses: actions/checkout@#{sha} # v4\n" }, true)
check(guard, { "ci.yaml" => "jobs: {test: {uses: 'owner/repo/.github/workflows/job.yml@#{sha}'}}\n" }, true)
[
  "actions/checkout@v4",
  "dtolnay/rust-toolchain@stable",
  "actions/checkout@abcdef0",
  "actions/checkout@#{sha}0",
  '${{ inputs.action }}',
  "./local-action",
].each do |ref|
  check(guard, { "ci.yml" => "jobs:\n  test:\n    steps:\n      - uses: '#{ref}'\n" }, false)
end
check(guard, { "ci.yaml" => "jobs: {test: {steps: [{uses: actions/checkout@v4}]}}\n" }, false)
check(guard, { "ci.yml" => "jobs:\n  test:\n    uses: >-\n      owner/repo/.github/workflows/job.yml@main\n" }, false)
check(guard, { "ci.yml" => "jobs: {test: {uses: null}}\n" }, false)

workflows = Dir.glob(File.join(root, ".github/workflows/*.{yml,yaml}"))
  .to_h { |file| [File.basename(file), File.read(file)] }
check(guard, workflows, true)
puts "Action pin guard: valid SHAs accepted; tags, branches, short SHAs and YAML variants rejected."
