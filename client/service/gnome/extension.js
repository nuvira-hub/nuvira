// NuviraHub unattended access.
//
// GNOME Shell inhibits remote access (gnome-remote-desktop, Mutter's
// RemoteDesktop/ScreenCast API) whenever its session mode does not allow
// screencasts: on the lock screen and at GDM's sign-in screen. Remote desktop
// sessions are closed at the lock and new ones are refused ("Session creation
// inhibited") until someone unlocks at the computer.
//
// With NuviraHub's unattended access the person has already chosen that this
// computer can be controlled without anyone at it, so while this extension is
// enabled (user session, lock screen, GDM's greeter) the inhibition does
// nothing. Disabling it gives GNOME's behaviour back.
//
// Mutter keeps a counter with no getter; GNOME Shell (ui/main.js) inhibits once
// when the session mode stops allowing screencasts and uninhibits once when it
// allows them again, and at GDM it does so before extensions are loaded. So the
// extension keeps the state GNOME Shell believes in (`_shellInhibited`) and
// makes the real counter match it again when disabled.

import * as Main from 'resource:///org/gnome/shell/ui/main.js';

export default class NuviraRemoteAccess {
    enable() {
        const controller = global.backend.get_remote_access_controller?.();
        if (!controller || this._controller)
            return;
        this._controller = controller;
        this._inhibit = controller.inhibit_remote_access;
        this._uninhibit = controller.uninhibit_remote_access;

        this._shellInhibited = !Main.sessionMode.allowScreencast && !global.backend.is_headless();
        if (this._shellInhibited)
            this._uninhibit.call(controller);

        controller.inhibit_remote_access = () => {
            this._shellInhibited = true;
        };
        controller.uninhibit_remote_access = () => {
            this._shellInhibited = false;
        };
        console.log(`NuviraHub: remote access kept on the lock and sign-in screens (${Main.sessionMode.currentMode})`);
    }

    disable() {
        const controller = this._controller;
        if (!controller)
            return;
        delete controller.inhibit_remote_access;
        delete controller.uninhibit_remote_access;
        if (controller.inhibit_remote_access !== this._inhibit) {
            controller.inhibit_remote_access = this._inhibit;
            controller.uninhibit_remote_access = this._uninhibit;
        }
        if (this._shellInhibited)
            this._inhibit.call(controller);
        this._controller = null;
        console.log('NuviraHub: remote access on the lock and sign-in screens back to GNOME default');
    }
}
