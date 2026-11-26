/**
 * Novoid Core - Main Entry
 *
 * This file orchestrates Alpine.js initialization.
 * Components are loaded via separate script files that register themselves
 * to window.Components before this script runs.
 */

document.addEventListener('alpine:init', () => {
    // Register Components (loaded from separate files via window.Components)
    Alpine.data('dashboard', window.Components.dashboard);
    Alpine.data('models', window.Components.models);
    Alpine.data('accountManager', window.Components.accountManager);
    Alpine.data('logsViewer', window.Components.logsViewer);
    Alpine.data('addAccountModal', window.Components.addAccountModal);

    // View Loader Directive
    Alpine.directive('load-view', (el, { expression }, { evaluate }) => {
        if (!window.viewCache) window.viewCache = new Map();

        // Evaluate the expression to get the actual view name (removes quotes)
        const viewName = evaluate(expression);

        if (window.viewCache.has(viewName)) {
            el.innerHTML = window.viewCache.get(viewName);
            Alpine.initTree(el);
            return;
        }

        fetch(`views/${viewName}.html?t=${Date.now()}`)
            .then(response => {
                if (!response.ok) throw new Error(`HTTP ${response.status}`);
                return response.text();
            })
            .then(html => {
                // Update cache (optional, or remove if we want always-fresh)
                // keeping cache for session performance, but initial load will now bypass browser cache
                window.viewCache.set(viewName, html);
                el.innerHTML = html;
                Alpine.initTree(el);
            })
            .catch(err => {
                console.error('Failed to load view:', viewName, err);
                const box = document.createElement('div');
                box.className = 'p-4 border border-red-500/50 bg-red-500/10 rounded-lg text-red-400 font-mono text-sm';
                const detail = document.createElement('span');
                detail.className = 'text-xs opacity-75';
                detail.textContent = err.message;
                box.append(`Error loading view: ${viewName}`, document.createElement('br'), detail);
                el.replaceChildren(box);
            });
    });

    // Main App Controller
    Alpine.data('app', () => ({
        get connectionStatus() {
            return Alpine.store('data')?.connectionStatus || 'connecting';
        },
        get loading() {
            return Alpine.store('data')?.loading || false;
        },

        sidebarOpen: window.innerWidth >= 1024,
        toggleSidebar() {
            this.sidebarOpen = !this.sidebarOpen;
        },

        init() {
            // Handle responsive sidebar transitions
            let lastWidth = window.innerWidth;
            let resizeTimeout = null;
            
            window.addEventListener('resize', () => {
                if (resizeTimeout) clearTimeout(resizeTimeout);
                
                resizeTimeout = setTimeout(() => {
                    const currentWidth = window.innerWidth;
                    const lgBreakpoint = 1024;
                    
                    // Desktop -> Mobile: Auto-close sidebar to prevent overlay blocking screen
                    if (lastWidth >= lgBreakpoint && currentWidth < lgBreakpoint) {
                        this.sidebarOpen = false;
                    }
                    
                    // Mobile -> Desktop: Auto-open sidebar (restore standard desktop layout)
                    if (lastWidth < lgBreakpoint && currentWidth >= lgBreakpoint) {
                        this.sidebarOpen = true;
                    }
                    
                    lastWidth = currentWidth;
                }, 150);
            });

            // Theme setup (respect saved preference)
            Alpine.store('global').applyTheme(Alpine.store('global').theme);

            // Chart Defaults
            if (typeof Chart !== 'undefined') {
                Chart.defaults.color = window.utils.getThemeColor('--color-text-dim');
                Chart.defaults.borderColor = window.utils.getThemeColor('--color-space-border');
                Chart.defaults.font.family = '"JetBrains Mono", monospace';
            }

            // Start Data Polling
            this.startAutoRefresh();
            document.addEventListener('refresh-interval-changed', () => this.startAutoRefresh());

            // Initial Fetch
            Alpine.store('data').fetchData();
        },

        refreshTimer: null,

        fetchData() {
            Alpine.store('data').fetchData();
        },

        startAutoRefresh() {
            if (this.refreshTimer) clearInterval(this.refreshTimer);
            const interval = parseInt(Alpine.store('settings')?.refreshInterval || 60);
            if (interval > 0) {
                this.refreshTimer = setInterval(() => Alpine.store('data').fetchData(), interval * 1000);
            }
        },

        t(key) {
            return Alpine.store('global')?.t(key) || key;
        },

        async addAccountWeb(reAuthEmail = null) {
            return Alpine.store('global').startOAuthFlow(reAuthEmail);
        }
    }));
});