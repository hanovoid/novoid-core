/**
 * Global Store for Novoid Core
 * Handles Translations, Toasts, and Shared Config
 */

document.addEventListener('alpine:init', () => {
    Alpine.store('global', {
        init() {
            // Hash-based routing
            const validTabs = ['dashboard', 'models', 'accounts', 'logs', 'settings'];
            const validSettingsTabs = ['ui', 'models', 'server'];
            const getHash = () => window.location.hash.substring(1);

            const parseHash = (hash) => {
                const [tab, subtab] = hash.split('/');
                return { tab, subtab };
            };

            // 1. Initial load from hash
            const { tab: initialTab, subtab: initialSubtab } = parseHash(getHash());
            if (validTabs.includes(initialTab)) {
                this.activeTab = initialTab;
                if (initialTab === 'settings' && validSettingsTabs.includes(initialSubtab)) {
                    this.settingsTab = initialSubtab;
                }
            }

            // 2. Sync State -> URL
            Alpine.effect(() => {
                if (!validTabs.includes(this.activeTab)) return;
                let target = this.activeTab;
                if (this.activeTab === 'settings' && this.settingsTab !== 'ui') {
                    target = `settings/${this.settingsTab}`;
                }
                if (getHash() !== target) {
                    window.location.hash = target;
                }
            });

            // 3. Sync URL -> State (Back/Forward buttons)
            window.addEventListener('hashchange', () => {
                const { tab, subtab } = parseHash(getHash());
                if (validTabs.includes(tab)) {
                    if (this.activeTab !== tab) {
                        this.activeTab = tab;
                    }
                    if (tab === 'settings') {
                        this.settingsTab = validSettingsTabs.includes(subtab) ? subtab : 'ui';
                    }
                }
            });

            // 4. Apply saved theme & fetch version from API
            this.applyTheme(this.theme);
            this.fetchVersion();
        },

        // Theme state ('light' | 'dark')
        theme: localStorage.getItem('novoid_theme') || 'light',

        applyTheme(mode) {
            const resolved = mode === 'dark' ? 'dark' : 'light';
            const root = document.documentElement;
            root.classList.toggle('dark', resolved === 'dark');
            root.setAttribute('data-theme', resolved === 'dark' ? 'novoid-dark' : 'novoid-light');
        },

        setTheme(mode) {
            const resolved = mode === 'dark' ? 'dark' : 'light';
            this.theme = resolved;
            localStorage.setItem('novoid_theme', resolved);
            this.applyTheme(resolved);
        },

        toggleTheme() {
            this.setTheme(this.theme === 'dark' ? 'light' : 'dark');
        },

        async fetchVersion() {
            try {
                const response = await fetch('/api/config');
                if (response.ok) {
                    const data = await response.json();
                    // Update maxAccounts in data store
                    if (data.config && typeof data.config.maxAccounts === 'number') {
                        Alpine.store('data').maxAccounts = data.config.maxAccounts;
                    }
                }
            } catch (error) {
                window.UILogger?.debug('Could not fetch version:', error);
            }
        },

        // App State
        version: '1.0.0',
        activeTab: 'dashboard',
        settingsTab: 'ui',
        webuiPassword: localStorage.getItem('novoid_webui_password') || '',

        // i18n
        lang: localStorage.getItem('app_lang') || 'en',
        translations: window.translations || {},

        // Toast Messages
        toast: null,

        // OAuth Progress
        oauthProgress: {
            active: false,
            current: 0,
            max: 120,
            mode: 'web',
            authUrl: '',
            authState: '',
            callbackInput: '',
            submitting: false,
            showManualInput: false,
            reAuthEmail: null,
            cancel: null,
            submitCallback: null,
            copyUrl: null,
            reopenWindow: null
        },

        isLocalBrowser() {
            const host = (window.location.hostname || '').toLowerCase();
            return host === 'localhost' || host === '127.0.0.1' || host === '::1' || host === '[::1]';
        },

        async startOAuthFlow(reAuthEmail = null) {
            if (typeof this.oauthProgress.cancel === 'function') {
                this.oauthProgress.cancel(true);
            }

            const password = this.webuiPassword;
            const localFlag = this.isLocalBrowser() ? '1' : '0';
            try {
                const params = new URLSearchParams({ local: localFlag });
                if (reAuthEmail) params.set('email', reAuthEmail);

                const { response, newPassword } = await window.utils.request(`/api/auth/url?${params.toString()}`, {}, password);
                if (newPassword) this.webuiPassword = newPassword;

                const data = await response.json();
                if (data.status !== 'ok') {
                    this.showToast(data.error || this.t('failedToGetAuthUrl'), 'error');
                    return;
                }

                // Sync active OAuth state with add-account-modal if open
                window.dispatchEvent(new CustomEvent('oauth-flow-started', {
                    detail: {
                        url: data.url,
                        state: data.state,
                        mode: data.mode || 'web'
                    }
                }));

                this.showToast(this.t('oauthInProgress'), 'info');

                let oauthWindow = window.open(data.url, 'google_oauth', 'width=600,height=700,scrollbars=yes');
                let pollCount = 0;
                const maxPolls = 60; // 120 seconds (2s interval)
                let finished = false;
                let pollInterval = null;

                const cleanup = (closePopup = false) => {
                    finished = true;
                    if (pollInterval) {
                        clearInterval(pollInterval);
                        pollInterval = null;
                    }
                    window.removeEventListener('message', onMessage);
                    this.oauthProgress.active = false;
                    this.oauthProgress.submitting = false;
                    this.oauthProgress.cancel = null;
                    this.oauthProgress.submitCallback = null;
                    if (closePopup && oauthWindow && !oauthWindow.closed) {
                        try { oauthWindow.close(); } catch (e) {}
                    }
                };

                const completeSuccess = async () => {
                    if (finished) return;
                    cleanup(true);
                    await Alpine.store('data').fetchData();
                    const actionKey = reAuthEmail ? 'accountReauthSuccess' : 'accountAddedSuccess';
                    this.showToast(this.t(actionKey), 'success');
                    document.getElementById('add_account_modal')?.close();
                };

                const onMessage = (event) => {
                    if (event.origin !== window.location.origin) return;
                    const msg = event.data;
                    if (!msg || msg.type !== 'oauth-complete' || msg.state !== data.state) return;

                    if (msg.status === 'ok') {
                        completeSuccess();
                    } else if (msg.status === 'error') {
                        cleanup(false);
                        this.showToast(msg.error || this.t('authFailed'), 'error');
                    }
                };
                window.addEventListener('message', onMessage);

                this.oauthProgress = {
                    active: true,
                    current: 0,
                    max: 120,
                    mode: data.mode || 'web',
                    authUrl: data.url,
                    authState: data.state,
                    callbackInput: '',
                    submitting: false,
                    showManualInput: data.mode === 'remote-loopback',
                    reAuthEmail,
                    copyUrl: async () => {
                        if (!this.oauthProgress.authUrl) return;
                        await navigator.clipboard.writeText(this.oauthProgress.authUrl);
                        this.showToast(this.t('linkCopied'), 'success');
                    },
                    reopenWindow: () => {
                        if (!this.oauthProgress.authUrl) return;
                        oauthWindow = window.open(this.oauthProgress.authUrl, 'google_oauth', 'width=600,height=700,scrollbars=yes');
                    },
                    submitCallback: async () => {
                        const input = (this.oauthProgress.callbackInput || '').trim();
                        if (!input || this.oauthProgress.submitting) return;
                        this.oauthProgress.submitting = true;
                        try {
                            const { response: compRes, newPassword: compNewPwd } = await window.utils.request('/api/auth/complete', {
                                method: 'POST',
                                headers: { 'Content-Type': 'application/json' },
                                body: JSON.stringify({
                                    callbackInput: input,
                                    state: this.oauthProgress.authState
                                })
                            }, this.webuiPassword);
                            if (compNewPwd) this.webuiPassword = compNewPwd;

                            const compData = await compRes.json();
                            if (compData.status === 'ok') {
                                await completeSuccess();
                            } else {
                                this.showToast(compData.error || this.t('authFailed'), 'error');
                            }
                        } catch (err) {
                            this.showToast(err.message || this.t('authFailed'), 'error');
                        } finally {
                            this.oauthProgress.submitting = false;
                        }
                    },
                    cancel: (silent = false) => {
                        if (finished) return;
                        cleanup(true);
                        if (!silent) {
                            this.showToast(this.t('oauthCancelled'), 'info');
                        }
                    }
                };

                const checkStatus = async () => {
                    try {
                        const { response: statusRes, newPassword: statusNewPwd } = await window.utils.request(
                            `/api/auth/status?state=${encodeURIComponent(data.state)}`,
                            {},
                            this.webuiPassword
                        );
                        if (statusNewPwd) this.webuiPassword = statusNewPwd;
                        const statusData = await statusRes.json();

                        if (statusData.status === 'ok') {
                            await completeSuccess();
                            return true;
                        }
                        if (statusData.status === 'error') {
                            cleanup(false);
                            this.showToast(statusData.error || this.t('authFailed'), 'error');
                            return true;
                        }
                    } catch (e) {
                        // Ignore transient poll errors
                    }
                    return false;
                };

                pollInterval = setInterval(async () => {
                    if (finished) return;

                    pollCount++;
                    this.oauthProgress.current = Math.min(120, pollCount * 2);

                    if (await checkStatus()) return;

                    // In web/local auto-callback mode, detect if popup was closed prematurely
                    if (this.oauthProgress.mode !== 'remote-loopback' && oauthWindow && oauthWindow.closed && !finished) {
                        // Give a short grace check in case callback landed right before close
                        setTimeout(async () => {
                            if (finished) return;
                            if (await checkStatus()) return;
                            cleanup(false);
                            this.showToast(this.t('oauthWindowClosed'), 'warning');
                        }, 400);
                        return;
                    }

                    if (pollCount >= maxPolls && !finished) {
                        cleanup(false);
                        this.showToast(this.t('oauthTimeout'), 'warning');
                    }
                }, 2000);
            } catch (e) {
                this.showToast(this.t('failedToStartOAuth') + ': ' + e.message, 'error');
            }
        },

        t(key, params = {}) {
            let str = this.translations[this.lang][key] || key;
            if (typeof str === 'string') {
                Object.keys(params).forEach(p => {
                    str = str.replace(`{${p}}`, params[p]);
                });
            }
            return str;
        },

        setLang(l) {
            this.lang = l;
            localStorage.setItem('app_lang', l);
        },

        showToast(message, type = 'info') {
            const id = Date.now();
            this.toast = { message, type, id };
            setTimeout(() => {
                if (this.toast && this.toast.id === id) this.toast = null;
            }, 3000);
        }
    });
});
