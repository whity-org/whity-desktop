<?php

declare(strict_types=1);

namespace Whity\Native;

use Whity\PluginHost\LoadedPlugin;

/**
 * The host's side of the bridge handshake. At boot, after the plugins are
 * loaded, it tells the bridge every plugin and the desktop capabilities it
 * declares, gets one handle back per plugin, and builds that plugin's
 * {@see NativeBridgeClient}. The host secret is used here once and kept
 * nowhere else.
 *
 * A plugin declares capabilities with `getDesktopCapabilities(): list<string>`
 * (duck-typed until the SDK interface lands in whity-core), for example
 * `['printer.print']`. A plugin without the method gets a handle with no
 * capabilities, so every call it makes is refused as `not_declared`.
 *
 * Until plugin containers are scoped per plugin, `for()` is reachable by any
 * plugin for any name; the per-plugin container PR closes that.
 */
final class NativeBridgeClients
{
    /** @var array<string, NativeBridgeClient> */
    private array $clients = [];

    /** @var array<string, list<string>> */
    private array $declared = [];

    /** @var list<string> */
    private array $unsupported = [];

    private ?string $failure = null;

    private function __construct(private readonly string $baseUrl)
    {
    }

    /**
     * Build from the environment the Rust sidecar sets. Without a bridge
     * (a host run by hand) every `for()` throws as unavailable.
     *
     * @param list<LoadedPlugin> $plugins
     */
    public static function boot(array $plugins): self
    {
        $baseUrl = rtrim((string) ($_ENV['WHITY_NATIVE_BRIDGE_URL'] ?? ''), '/');
        $secret = (string) ($_ENV['WHITY_NATIVE_BRIDGE_SECRET'] ?? '');
        $self = new self($baseUrl);
        if ($baseUrl === '' || strlen($secret) < 32) {
            $self->failure = 'no native bridge: WHITY_NATIVE_BRIDGE_URL or WHITY_NATIVE_BRIDGE_SECRET is not set';

            return $self;
        }

        $declarations = [];
        foreach ($plugins as $loaded) {
            $name = $loaded->plugin->getName();
            $capabilities = self::declaredCapabilities($loaded);
            $self->declared[$name] = $capabilities;
            $declarations[] = ['name' => $name, 'capabilities' => $capabilities];
        }

        try {
            $answer = NativeBridgeHttp::postJson(
                $baseUrl . '/handles',
                ['X-Whity-Host-Secret: ' . $secret],
                ['plugins' => $declarations],
                10,
            );
        } catch (NativeBridgeRefusedException|NativeBridgeUnavailableException $e) {
            $self->failure = 'the native bridge refused the handle request: ' . $e->getMessage();
            error_log('[php-host] ' . $self->failure);

            return $self;
        }

        $handles = is_array($answer['handles'] ?? null) ? $answer['handles'] : [];
        foreach ($handles as $name => $handle) {
            if (is_string($name) && is_string($handle) && $handle !== '') {
                $self->clients[$name] = new NativeBridgeClient($baseUrl, $handle);
            }
        }
        $unsupported = $answer['unsupported'] ?? [];
        $self->unsupported = is_array($unsupported) ? array_values(array_filter($unsupported, 'is_string')) : [];
        foreach ($self->unsupported as $line) {
            error_log("[php-host] desktop capability with no driver in this build: {$line}");
        }

        return $self;
    }

    /**
     * The client for one plugin, by its declared name.
     *
     * @throws NativeBridgeUnavailableException When there is no bridge, or no
     *         handle for that plugin.
     */
    public function for(string $pluginName): NativeBridgeClient
    {
        if ($this->failure !== null) {
            throw new NativeBridgeUnavailableException($this->failure);
        }
        if (!isset($this->clients[$pluginName])) {
            throw new NativeBridgeUnavailableException("no native bridge handle for plugin {$pluginName}");
        }

        return $this->clients[$pluginName];
    }

    /**
     * What the host told the bridge, for the status screen.
     *
     * @return array{available: bool, failure: ?string, plugins: array<string, list<string>>, unsupported: list<string>}
     */
    public function report(): array
    {
        return [
            'available' => $this->failure === null,
            'failure' => $this->failure,
            'plugins' => $this->declared,
            'unsupported' => $this->unsupported,
        ];
    }

    /** @return list<string> */
    private static function declaredCapabilities(LoadedPlugin $loaded): array
    {
        $plugin = $loaded->plugin;
        if (!method_exists($plugin, 'getDesktopCapabilities')) {
            return [];
        }
        try {
            /** @var mixed $declared */
            $declared = $plugin->getDesktopCapabilities();
        } catch (\Throwable $e) {
            error_log("[php-host] {$loaded->fqcn}::getDesktopCapabilities() threw: {$e->getMessage()}");

            return [];
        }
        if (!is_array($declared)) {
            return [];
        }
        $clean = [];
        foreach ($declared as $capability) {
            if (is_string($capability) && preg_match('/^[a-z][a-z0-9-]*\.[a-z][a-z0-9_]*$/', $capability) === 1) {
                $clean[] = $capability;
            } else {
                error_log("[php-host] {$loaded->fqcn} declared a malformed desktop capability; ignored");
            }
        }

        return array_values(array_unique($clean));
    }
}
