# frozen_string_literal: true

require 'digest'
require 'json'

module Kori
  # Slash-command schema shared with the Rust bot (src/services/command_sync.rs).
  module Commands
    SCHEMA_PATH = File.join(ROOT, 'shared', 'commands.json')
    EXPECTED = %w[ticket add remove close reclaim].freeze

    module_function

    def definitions = JSON.parse(File.read(SCHEMA_PATH, encoding: 'UTF-8'))
    def names(defs = definitions) = defs.map { _1['name'] }

    def canonicalize(value)
      case value
      when Array then value.map { canonicalize(_1) }
      when Hash then value.sort_by { |key, _| key }.to_h { |key, child| [key, canonicalize(child)] }
      else value
      end
    end

    # sha256 of sorted-key compact JSON; identical to the Rust fingerprint.
    def fingerprint(defs = definitions) = Digest::SHA256.hexdigest(JSON.generate(canonicalize(defs)))

    def assert_schema!(defs = definitions)
      raise 'Kori command schema must contain exactly ticket, add, remove, close, and reclaim.' unless names(defs) == EXPECTED

      ticket = defs.find { _1['name'] == 'ticket' }
      raise 'ticket must not have subcommands or options.' if ticket && !(ticket['options'] || []).empty?
    end

    # Option fields Discord echoes back, with Discord's default for `required`.
    def normalize_options(options)
      (options || []).map do |option|
        { 'type' => option['type'], 'name' => option['name'], 'description' => option['description'],
          'required' => option['required'] ? true : false }
      end
    end
  end
end
