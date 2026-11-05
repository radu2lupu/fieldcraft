// genmedia: synthetic test media (images + a 30 s WAV clip). Standard library only.
package main

import (
	"encoding/binary"
	"fmt"
	"image"
	"image/color"
	"image/jpeg"
	"image/png"
	"math"
	"math/rand"
	"os"
)

func render(size int, seed int64) *image.RGBA {
	r := rand.New(rand.NewSource(seed))
	img := image.NewRGBA(image.Rect(0, 0, size, size))
	c0 := color.RGBA{uint8(r.Intn(256)), uint8(r.Intn(256)), uint8(r.Intn(256)), 255}
	c1 := color.RGBA{uint8(r.Intn(256)), uint8(r.Intn(256)), uint8(r.Intn(256)), 255}
	for y := 0; y < size; y++ {
		for x := 0; x < size; x++ {
			t := float64(x+y) / float64(2*size)
			img.Set(x, y, color.RGBA{uint8(float64(c0.R)*(1-t) + float64(c1.R)*t), uint8(float64(c0.G)*(1-t) + float64(c1.G)*t), uint8(float64(c0.B)*(1-t) + float64(c1.B)*t), 255})
		}
	}
	for k := 0; k < 12; k++ { // random filled rectangles and discs
		col := color.RGBA{uint8(r.Intn(256)), uint8(r.Intn(256)), uint8(r.Intn(256)), 255}
		cx, cy, rad := r.Intn(size), r.Intn(size), 20+r.Intn(size/6)
		for y := cy - rad; y < cy+rad; y++ {
			for x := cx - rad; x < cx+rad; x++ {
				if x >= 0 && y >= 0 && x < size && y < size && (k%2 == 0 || (x-cx)*(x-cx)+(y-cy)*(y-cy) < rad*rad) {
					img.Set(x, y, col)
				}
			}
		}
	}
	return img
}

func main() {
	dir := os.Args[1]
	os.MkdirAll(dir, 0o755)
	for i := 0; i < 20; i++ {
		size := 896
		if i%2 == 1 {
			size = 640
		}
		img := render(size, int64(i+1))
		if i < 10 {
			f, _ := os.Create(fmt.Sprintf("%s/img%02d_%d.png", dir, i, size))
			png.Encode(f, img)
			f.Close()
		} else {
			f, _ := os.Create(fmt.Sprintf("%s/img%02d_%d.jpg", dir, i, size))
			jpeg.Encode(f, img, &jpeg.Options{Quality: 85})
			f.Close()
		}
	}
	// 30 s, 16 kHz mono 16-bit WAV: swept tones with amplitude modulation
	const sr, secs = 16000, 30
	n := sr * secs
	data := make([]int16, n)
	for i := range data {
		t := float64(i) / sr
		f := 220 + 180*math.Sin(2*math.Pi*0.2*t)
		data[i] = int16(9000 * math.Sin(2*math.Pi*f*t) * (0.6 + 0.4*math.Sin(2*math.Pi*3*t)))
	}
	f, _ := os.Create(dir + "/clip30s.wav")
	defer f.Close()
	hdr := make([]byte, 44)
	copy(hdr[0:], "RIFF")
	binary.LittleEndian.PutUint32(hdr[4:], uint32(36+n*2))
	copy(hdr[8:], "WAVEfmt ")
	binary.LittleEndian.PutUint32(hdr[16:], 16)
	binary.LittleEndian.PutUint16(hdr[20:], 1)
	binary.LittleEndian.PutUint16(hdr[22:], 1)
	binary.LittleEndian.PutUint32(hdr[24:], sr)
	binary.LittleEndian.PutUint32(hdr[28:], sr*2)
	binary.LittleEndian.PutUint16(hdr[32:], 2)
	binary.LittleEndian.PutUint16(hdr[34:], 16)
	copy(hdr[36:], "data")
	binary.LittleEndian.PutUint32(hdr[40:], uint32(n*2))
	f.Write(hdr)
	binary.Write(f, binary.LittleEndian, data)
}
