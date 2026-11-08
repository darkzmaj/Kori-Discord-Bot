# frozen_string_literal: true

# Kori operations tooling: migrations, command synchronization and diagnostics.
# The bot itself runs in Rust (../src); both sides share db/migrations and shared/commands.json.
module Kori
  ROOT = File.expand_path('../..', __dir__)
end

require_relative 'kori/config'
require_relative 'kori/logger'
require_relative 'kori/database'
require_relative 'kori/commands'
require_relative 'kori/discord_rest'
require_relative 'kori/command_sync'
