## Temporary EVDI hardware trial

The private-RAM benchmark found roughly 70% less CPU time in the capture copy
with 128-row batching. This procedure tests the same patch on the actual
GPU-backed desktop framebuffer. It requires one logout to replace the loaded
EVDI module because X11 and DisplayLinkManager hold the original open.

The [switch tool](../tools/gpu/evdi-module-trial.py) preserves a copy of the
installed original module and loads the checked candidate directly from a
root-owned staging directory. It does not install a module, alter DKMS or
update the initramfs. The candidate starts with `copy_batch_rows=0`, which
keeps the original row loop. An ordinary reboot loads the installed original
module again.

### Loading the temporary module

1. Save your work and close applications that must not be interrupted because the next command signs you out of the graphical session
2. Run the switch from your desktop terminal:
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 /home/cameronball/Projects/Vino/tools/gpu/evdi-module-trial.py switch --build /home/cameronball/Projects/Vino/gpu-artifacts/evdi-copy-20261020
    ```
3. Sign back into the usual KDE X11 session and check the result:
    ```bash
    python3 /home/cameronball/Projects/Vino/tools/gpu/evdi-module-trial.py status
    ```
4. **Verify** the loaded source version is `9232839421842403DFD2636`, `copy_batch_rows` is `"0"`, both display services are active and the recovery record has `switched: true` with no errors
5. Restore the saved monitor positions if the new login changed them and **verify** the laptop, Dell and BenQ show the normal desktop:
    ```bash
    python3 /home/cameronball/Projects/Vino/tools/gpu/evdi-module-trial.py restore-layout
    ```

The switch runs in a system service, so logging out does not kill it. It
temporarily masks DisplayLink, stops DisplayLink and SDDM, performs a normal
EVDI unload, loads the candidate with batching disabled, removes its runtime
mask and starts both display services. It preserves the original device-count
and logging parameters. It never forces an unload or removes the AMD GPU
driver.

A failure or timeout starts a separate recovery service which attempts to
restore the saved original module and restart the login screen. The switch
has a 120-second timeout, and recovery has its own 120-second timeout. State
and outcome records are kept in the printed **/var/tmp/vino-evdi-module-…**
directory, with the latest directory recorded in
**/run/vino-evdi-module-trial.json**. Service startup success alone does not
prove that every monitor has recovered; step 5 checks that before a live test.

If the login screen has not returned after recovery, use Ctrl+Alt+F3 to sign
into a text console and reboot with `sudo reboot`; the installed original
module remains available. These recovery mechanisms cannot repair a kernel
that has stopped responding.

### First desktop check

1. After the normal desktop is confirmed, start the 90-second GPU desktop trial with 128-row batching and CPU profiling:
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 /home/cameronball/Projects/Vino/tools/gpu/live-trial.py run --encoder gpu --source desktop --fps 60 --duration 90 --profile-cpu --evdi-copy-rows 128
    ```
2. Move a window fully onto the Dell, drag it around, scroll some text and **verify** the image stays correct, movement stays smooth and the cursor leaves no trails
3. Let the trial finish or use Ctrl+C to finish early and **verify** both dock monitors return to their normal desktops with `evdi_copy_rows_restored: 0` and no recovery errors

The existing trial temporarily places the Dell to the right of the laptop
and turns off the BenQ. The separate capture thread continues acknowledging
EVDI independently of presentation. Batching is enabled only after the
official DisplayLink service has stopped, and recovery resets it before
restarting that service. The helper rejects a different loaded module or a
nonzero starting value.

The original module and the candidate with batching disabled have already
been compared in the component benchmark. For an on-device baseline using
the same candidate build, repeat the trial with `--evdi-copy-rows 0` and
comparable activity. Manual activity varies, so compare copy/capture CPU per
capture as well as process CPU and update timing; a matched replay is needed
for a controlled whole-driver or power claim. The first trial primarily
establishes real-framebuffer correctness, smoothness and the location of CPU
costs.

### Returning to the installed driver

After each live trial, the temporary module remains loaded with batching
disabled. The next ordinary reboot restores the installed module. To restore
the saved original immediately, save work first because this command signs
out the desktop again:

```bash
sudo --preserve-env=DISPLAY,XAUTHORITY python3 /home/cameronball/Projects/Vino/tools/gpu/evdi-module-trial.py restore
```

After signing back in, the status command should report original source
version `12258322BFFCAAE41410871`, no batch parameter, active display services
and a successful switch record. The layout command can restore the saved
monitor positions again. The restore operation uses the preserved original
module and the same supervised recovery mechanism.

### Preparation checks

The original, candidate and isolated benchmark modules were built for kernel
**7.0.0-34-generic**. The installed original compressed module has SHA-256
`2f3e247c3597eb95ed4f9ac76057f7cf2cd14cdd2232570396b774d210c5173b`;
the candidate has SHA-256
`db30a66884ff5e0b5c74ae03b454c15aa37e11e29208ff1e98e19d5faa56a7ca`.
The switch checks the kernel, source versions and staged bytes before use.
Secure Boot is already disabled on this machine; the tool does not change it.

Preparation passes ten module-switch policy tests and seven new batching
lifecycle tests, alongside the existing live-trial/comparison tests. The
policy cases cover a busy module, failed candidate load, failed display
restart, altered module bytes, an unexpected module, and retention of another
runtime override. Actual systemd user-service simulations pass normal exit,
failure and timeout, including dispatch to a separate recovery service.
Those simulations do not unload a real driver or log out the desktop.

The preparation preflight confirmed the original module, all three saved
monitor positions, active display services and the Radeon 780M GPU worker.
Preparation evidence and source snapshots are under
**gpu-artifacts/evdi-copy-20261020/live-switch-preparation/**.

The actual switch and first 128-row desktop trial subsequently completed
successfully on 20 October 2026. The user reported good visual behavior.
Recovery restored batching to zero, and an independent check confirmed all
three saved monitor positions and the official desktop services. The
temporary candidate remains loaded with batching disabled; the installed
module is unchanged. See the [hardware result](gpu-evdi-copy-investigation.md#first-hardware-result)
and **gpu-artifacts/evdi-copy-20261020/live-n7g38clw/** for the recorded
CPU profile, recovery evidence and comparison limitations.
