# Dependencies
Only works on Linux. You might need to install libclang>=3.9:
`sudo apt install libclang-3.9-dev`

## Udev rules
This worked on my Raspi
`/etc/udev/rules.d/51-usb-device.rules`:
```udev
SUBSYSTEM=="usb", ATTRS{idVendor}=="0416", ATTRS{idProduct}=="5011", GROUP="plugdev", TAG+="uaccess"
```

## Arch linux tip:
I had success using:
```sh
sudo groupadd dialout
sudo usermod -a -G dialout $USER
sudo modprobe -r usblp
```
