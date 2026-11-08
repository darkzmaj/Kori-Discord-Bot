# frozen_string_literal: true

require 'minitest/autorun'
require 'tmpdir'
require_relative '../lib/kori'

class TestCommands < Minitest::Test
  def test_schema_is_the_five_guild_commands
    Kori::Commands.assert_schema!
    assert_equal %w[ticket add remove close reclaim], Kori::Commands.names
  end

  # Same value is asserted in src/services/command_sync.rs, so Rust and Ruby agree on sync state.
  def test_fingerprint_matches_rust
    assert_equal '8449abb8d61d4f2a428ec3ad6d462b6ae7a85dcaccce0882327d06543278ca36', Kori::Commands.fingerprint
  end

  def test_ticket_with_options_is_rejected
    defs = Kori::Commands.definitions
    defs.first['options'] = [{ 'type' => 3, 'name' => 'x', 'description' => 'x' }]
    assert_raises(RuntimeError) { Kori::Commands.assert_schema!(defs) }
  end

  def test_names_mention_kori
    text = File.read(Kori::Commands::SCHEMA_PATH, encoding: 'UTF-8')
    assert_match(/Kori/, text)
  end
end

class TestDatabase < Minitest::Test
  def test_migrations_apply_once_and_sync_state_round_trips
    Dir.mktmpdir do |dir|
      db = Kori::Database.open(File.join(dir, 'nested', 'kori.sqlite'))
      Kori::Database.migrate(db)
      assert_equal Kori::Database.migrations.size, db.get_first_value('SELECT COUNT(*) FROM schema_migrations')
      assert_nil Kori::Database.command_sync_state(db, 'app')
      Kori::Database.save_command_sync_state(db, 'app', 'abc', false)
      Kori::Database.save_command_sync_state(db, 'app', 'def', true)
      state = Kori::Database.command_sync_state(db, 'app')
      assert_equal ['def', 1], [state['schema_fingerprint'], state['guild_cleanup_complete']]
      refute Kori::Database.ticket_staff?(db, 'g', ['r'])
      db.close
    end
  end
end

class TestPermissions < Minitest::Test
  GUILD = { 'id' => 'g', 'owner_id' => 'owner',
            'roles' => [{ 'id' => 'g', 'permissions' => (1 << 31).to_s }, { 'id' => 'admin', 'permissions' => '8' }] }.freeze

  def test_effective_permissions
    member = ->(id, roles) { { 'user' => { 'id' => id }, 'roles' => roles } }
    assert_equal 1 << 31, Kori::CommandSync.effective_permissions(GUILD, member.call('u', []))
    assert_equal Kori::CommandSync::ALL_PERMISSIONS, Kori::CommandSync.effective_permissions(GUILD, member.call('u', ['admin']))
    assert_equal Kori::CommandSync::ALL_PERMISSIONS, Kori::CommandSync.effective_permissions(GUILD, member.call('owner', []))
  end
end

class TestLogger < Minitest::Test
  def test_redacts_plain_and_versioned_webhook_urls_and_tokens
    plain = 'https://discord.com/api/webhooks/123456789012345678/abc-DEF_1'
    versioned = 'https://discord.com/api/v10/webhooks/123456789012345678/secret-Token_9'
    # Split so secret scanners don't flag this fake token.
    token = 'MTIzNDU2Nzg5MDEyMzQ1Njc4OTA' + '.GhIjKl.abcdefghijklmnopqrstuvwxyz12'
    assert_equal '[redacted webhook] [redacted webhook] [redacted token]',
                 Kori::Logger.redact("#{plain} #{versioned} #{token}")
  end
end
