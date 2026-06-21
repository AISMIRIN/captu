Rounded M+ 1m for ARIB (2012-12-19)->合成元更新に追従(2013-12-05)

■概要
このフォントは[1]で公開されている丸ゴシック化されたM+に、[2]で公開されている和田
丸ゴのJIS第三水準までの漢字およびUnicode5.2+外字領域のARIB文字を補ったものです。
[1]「自家製 Rounded M+」1m regular 1.056.20130705,
   https://sites.google.com/site/roundedmplus/
[2]和田研中丸ゴシック2004ARIB(Unicode版)(等幅フォント) 4.30; 4.3.0.0,
   http://sourceforge.jp/projects/jis2004/
フォント作成はXubuntu12.10上のfontforge20110225で行いました。両フォントを同じフ
ォルダに配置して下記コマンドで添付のものと同じフォントが生成されるはずです。
$ fontforge -script ./rounded-mplus-1m-arib.pe

■改変点
1.和田丸ゴと合わせるためにemを1024unitに変更
2.ディセントの深いgｇjｊpｐqｑyｙをY方向に90%に圧縮して+45unit移動
3.半角設計の文字の一部を和田丸ゴのものに置換("-copylist.txt"の[Detach list])
4.たりない文字を和田丸ゴからコピー([Copy list])
5.外字領域にARIB文字を配置([Refer or copy list])。ここで和田丸ゴのARIB STD-B24
  93区13,14点のグリフ逆転(No,Tel→Tel,〒になっている)も補正
6.表示上のアセントとディセントを縮小(→上下のダイアクリティカルマークなどが切れ
  る場合がある)

■ライセンス
このフォントはフリーフォントです。合成元各フォントの下記ライセンスのうち、ベース
とした「自家製 Rounded M+ ライセンス」を継承します。

---引用開始 自家製 Rounded M+ ライセンス---
These fonts are free software.
Unlimited permission is granted to use, copy, and distribute them, with
or without modification, either commercially or noncommercially.
THESE FONTS ARE PROVIDED "AS IS" WITHOUT WARRANTY.
---引用終了---

---引用開始 和田研中丸ゴシック2004ARIB ライセンス---
このフォントはフリーフォントです。
無償で使用可能です。
商用・非商用いずれでもお使い頂けます。
このフォントは再配布が可能です。
このフォントは改変が可能です。
商用非商用に関わらずＯＳなどにインストールした状態での配布も可能です。 
また、他のソフトに添付や組み込んだ形での配布も可能です。 
このフォントを使用して作成したものに、このフォントを使用した旨の記載は
必要ありません。 
このフォントを使用して作成したものに、このフォントの規定が継承されるこ
とはありません。 
このフォントを使用や再配布をするにあたって当方への確認は必要ありません。
---引用終了---
